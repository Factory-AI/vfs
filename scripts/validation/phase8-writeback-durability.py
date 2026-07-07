#!/usr/bin/env python3
"""Phase 8 writeback durability crash/reopen gate.

Writes bytes through a fresh AgentFS FUSE mount, fsyncs the file and parent
directory, SIGKILLs the mount process, remounts the same DB, and requires the
bytes to be present with portable integrity and an unchanged base tree.
"""

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
from typing import Any, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib.common import (  # noqa: E402
    env_flag,
    git_commit,
    parse_json_stdout,
    positive_float,
    positive_int,
    resolve_agentfs_bin,
    run_subprocess,
    tail_text,
    terminate_process_tree,
)

HASH_BLOCK_BYTES = 1024 * 1024


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify fsynced AgentFS writes survive mount SIGKILL and remount."
    )
    parser.add_argument("--write-bytes", type=positive_int, default=8192)
    parser.add_argument(
        "--agentfs-bin",
        default=os.environ.get("AGENTFS_BIN"),
        help="agentfs executable path/name (default: repo target binary, building cli if needed)",
    )
    parser.add_argument(
        "--timeout",
        type=positive_float,
        default=positive_float(os.environ.get("PHASE8_WRITEBACK_TIMEOUT", "90")),
    )
    parser.add_argument("--session", default=None)
    parser.add_argument("--keep-temp", action="store_true", default=env_flag("PHASE8_KEEP_TEMP"))
    parser.add_argument("--output", help="write JSON result to this file")
    parser.add_argument("--json-indent", type=int, default=2)
    return parser.parse_args(argv)


def prepare_environment(temp_root: Path) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    env.setdefault("NO_COLOR", "1")
    home = temp_root / "home"
    for path in (home, home / ".config", home / ".cache", home / ".local" / "share"):
        path.mkdir(parents=True, exist_ok=True)
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(home / ".config")
    env["XDG_CACHE_HOME"] = str(home / ".cache")
    env["XDG_DATA_HOME"] = str(home / ".local" / "share")
    tmp = temp_root / "tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(tmp)
    env["TMP"] = str(tmp)
    env["TEMP"] = str(tmp)
    return env


def deterministic_bytes(length: int) -> bytes:
    out = bytearray()
    index = 0
    while len(out) < length:
        out.extend(hashlib.sha256(f"agentfs-phase8-durable-{index}".encode()).digest())
        index += 1
    return bytes(out[:length])


def create_base_fixture(root: Path) -> None:
    root.mkdir(parents=True, exist_ok=True)
    (root / "base_sentinel.txt").write_bytes(deterministic_bytes(4096))
    nested = root / "nested"
    nested.mkdir()
    (nested / "read_only.txt").write_text("phase8 base must remain unchanged\n", encoding="utf-8")


def tree_hash(root: Path) -> dict[str, Any]:
    digest = hashlib.sha256()
    file_count = 0
    dir_count = 0
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
    return {"sha256": digest.hexdigest(), "files": file_count, "directories": dir_count, "bytes": total_bytes}


def is_mountpoint(path: Path) -> bool:
    mountpoint_bin = shutil.which("mountpoint")
    if mountpoint_bin:
        return subprocess.run(
            [mountpoint_bin, "-q", str(path)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode == 0
    try:
        return path.is_mount()
    except OSError:
        return False


def collect_process(proc: subprocess.Popen[str]) -> dict[str, Any]:
    try:
        stdout, stderr = proc.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        terminate_process_tree(proc)
        stdout, stderr = proc.communicate(timeout=5)
    return {
        "returncode": proc.returncode,
        "stdout_tail": tail_text(stdout),
        "stderr_tail": tail_text(stderr),
        "stdout_bytes": len((stdout or "").encode("utf-8", errors="replace")),
        "stderr_bytes": len((stderr or "").encode("utf-8", errors="replace")),
    }


def unmount(mountpoint: Path) -> list[dict[str, Any]]:
    attempts = []
    for command in ("fusermount3", "fusermount"):
        binary = shutil.which(command)
        if not binary:
            continue
        for args in (["-u", str(mountpoint)], ["-uz", str(mountpoint)]):
            proc = subprocess.run(
                [binary] + args,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            attempts.append(
                {
                    "argv": [binary] + args,
                    "returncode": proc.returncode,
                    "stdout_tail": tail_text(proc.stdout),
                    "stderr_tail": tail_text(proc.stderr),
                }
            )
            if proc.returncode == 0 or not is_mountpoint(mountpoint):
                return attempts
    return attempts


def start_mount(agentfs_bin: str, id_or_path: Any, mountpoint: Path, env: dict[str, str], timeout: float) -> tuple[subprocess.Popen[str], dict[str, Any]]:
    try:
        mountpoint.mkdir(parents=True, exist_ok=True)
    except FileExistsError:
        pass
    argv = [
        agentfs_bin,
        "mount",
        str(id_or_path),
        str(mountpoint),
        "--foreground",
        "--backend",
        "fuse",
    ]
    proc = subprocess.Popen(
        argv,
        cwd=str(mountpoint.parent),
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    started = time.perf_counter()
    deadline = started + timeout
    while time.perf_counter() < deadline:
        if proc.poll() is not None:
            output = collect_process(proc)
            raise RuntimeError(f"mount exited before becoming ready: {output}")
        if is_mountpoint(mountpoint):
            return proc, {"argv": argv, "ready_seconds": time.perf_counter() - started}
        time.sleep(0.05)
    terminate_process_tree(proc)
    output = collect_process(proc)
    raise RuntimeError(f"mount did not become ready within {timeout} seconds: {output}")


def stop_mount_clean(proc: subprocess.Popen[str], mountpoint: Path) -> dict[str, Any]:
    attempts = unmount(mountpoint)
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        terminate_process_tree(proc)
    output = collect_process(proc)
    return {"unmount_attempts": attempts, "process": output, "mounted_after": is_mountpoint(mountpoint)}


def kill_mount(proc: subprocess.Popen[str], mountpoint: Path) -> dict[str, Any]:
    killed = False
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGKILL)
        killed = True
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
    output = collect_process(proc)
    attempts = unmount(mountpoint)
    return {
        "sent_sigkill": killed,
        "process": output,
        "unmount_attempts": attempts,
        "mounted_after": is_mountpoint(mountpoint),
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
            result["portability_status"] = {"portable": partial_rows == 0, "partial_origin_rows": partial_rows}
            return result
        finally:
            conn.close()
    except Exception as exc:
        return {"inspectable": False, "reason": str(exc), "path": str(db_path)}


def sidecar_status(db_path: Path) -> dict[str, Any]:
    wal = db_path.with_name(db_path.name + "-wal")
    if wal.exists() and wal.stat().st_size == 0:
        wal.unlink()
    shm = db_path.with_name(db_path.name + "-shm")
    if shm.exists():
        shm.unlink()

    artifacts = []
    for path in (db_path, db_path.with_name(db_path.name + "-wal"), db_path.with_name(db_path.name + "-shm")):
        artifacts.append({"path": str(path), "exists": path.exists(), "bytes": path.stat().st_size if path.exists() else 0})
    sidecars = [item for item in artifacts if item["path"].endswith(("-wal", "-shm"))]
    return {
        "artifacts": artifacts,
        "no_nonempty_sidecars": all(int(item["bytes"]) == 0 for item in sidecars),
        "strict_no_sidecar_files": all(not item["exists"] for item in sidecars),
    }


def run_integrity(agentfs_bin: str, db_path: Path, cwd: Path, env: dict[str, str], timeout: float) -> dict[str, Any]:
    run = run_subprocess(
        [agentfs_bin, "integrity", str(db_path), "--json", "--require-portable"],
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


def fsync_directory(path: Path) -> None:
    fd = os.open(str(path), os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def default_output_path() -> Path:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return Path(tempfile.gettempdir()) / f"agentfs-phase8-writeback-durability-{stamp}-{uuid.uuid4().hex[:8]}.json"


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    output_path = Path(args.output).expanduser() if args.output else default_output_path()

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        temp_root = Path(tempfile.mkdtemp(prefix="agentfs-phase8-writeback-durable-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(
            prefix="agentfs-phase8-writeback-durable-",
            ignore_cleanup_errors=True,
        )
        temp_root = Path(temp_manager.name)

    mount_proc: Optional[subprocess.Popen[str]] = None
    remount_proc: Optional[subprocess.Popen[str]] = None
    mountpoint: Optional[Path] = None
    exit_code = 0
    result: dict[str, Any]
    try:
        agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env = prepare_environment(temp_root)
        session = args.session or f"phase8-durable-{uuid.uuid4().hex}"
        base_root = temp_root / "base"
        create_base_fixture(base_root)
        base_before = tree_hash(base_root)
        db_path = temp_root / ".agentfs" / f"{session}.db"

        init_run = run_subprocess(
            [agentfs_bin, "init", "--force", "--base", str(base_root), session],
            temp_root,
            env,
            args.timeout,
        )
        if init_run["returncode"] != 0:
            raise RuntimeError(f"agentfs init failed: {init_run['stderr_tail']}")

        mountpoint = temp_root / "mnt"
        mountpoint.mkdir(parents=True, exist_ok=True)
        mount_proc, mount_start = start_mount(agentfs_bin, session, mountpoint, env, args.timeout)
        expected = deterministic_bytes(args.write_bytes)
        write_path = mountpoint / "durable.bin"
        started_write = time.perf_counter()
        with write_path.open("wb", buffering=0) as handle:
            written = handle.write(expected)
            handle.flush()
            os.fsync(handle.fileno())
        fsync_directory(mountpoint)
        write_record = {
            "path": str(write_path),
            "bytes_requested": len(expected),
            "bytes_written": written,
            "duration_seconds": time.perf_counter() - started_write,
            "sha256": hashlib.sha256(expected).hexdigest(),
        }

        kill_record = kill_mount(mount_proc, mountpoint)
        mount_proc = None

        remount_proc, remount_start = start_mount(agentfs_bin, session, mountpoint, env, args.timeout)
        read_error = None
        read_bytes = b""
        try:
            read_bytes = write_path.read_bytes()
        except Exception as exc:
            read_error = str(exc)
        remount_read = {
            "path": str(write_path),
            "error": read_error,
            "bytes": len(read_bytes),
            "sha256": hashlib.sha256(read_bytes).hexdigest() if read_error is None else None,
            "matches_expected": read_error is None and read_bytes == expected,
        }
        clean_unmount = stop_mount_clean(remount_proc, mountpoint)
        remount_proc = None

        integrity = run_integrity(agentfs_bin, db_path, temp_root, env, args.timeout)
        db_inspect = inspect_db(db_path)
        sidecars = sidecar_status(db_path)
        base_after = tree_hash(base_root)
        base_unchanged = base_before["sha256"] == base_after["sha256"]

        passed = (
            init_run["returncode"] == 0
            and write_record["bytes_written"] == len(expected)
            and kill_record.get("sent_sigkill") is True
            and remount_read["matches_expected"] is True
            and integrity.get("ok") is True
            and db_inspect.get("inspectable") is True
            and db_inspect.get("portability_status", {}).get("portable") is True
            and sidecars["strict_no_sidecar_files"] is True
            and base_unchanged
        )
        if not passed:
            exit_code = 1

        result = {
            "schema_version": 1,
            "benchmark": "phase8-writeback-durability",
            "git_commit": git_commit(repo_root),
            "parameters": {"write_bytes": args.write_bytes, "timeout_seconds": args.timeout},
            "agentfs": {"bin": agentfs_bin, "session": session, "db_path": str(db_path)},
            "summary": {
                "passed": passed,
                "bytes_present_after_remount": remount_read["matches_expected"],
                "sent_sigkill": kill_record.get("sent_sigkill"),
                "integrity_ok": integrity.get("ok"),
                "base_unchanged": base_unchanged,
                "strict_no_sidecar_files": sidecars["strict_no_sidecar_files"],
            },
            "runs": {
                "init": init_run,
                "mount": mount_start,
                "write_fsync": write_record,
                "kill": kill_record,
                "remount": remount_start,
                "remount_read": remount_read,
                "clean_unmount": clean_unmount,
            },
            "database": {"inspect_after": db_inspect, "integrity": integrity, "sidecars_after_integrity": sidecars},
            "base_tree": {"before": base_before, "after": base_after, "unchanged": base_unchanged},
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "phase8-writeback-durability",
            "error": str(exc),
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }
    finally:
        for proc in (mount_proc, remount_proc):
            if proc is not None and proc.poll() is None:
                terminate_process_tree(proc)
        if mountpoint is not None:
            try:
                unmount(mountpoint)
            except Exception:
                pass

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(payload, encoding="utf-8")
    sys.stdout.write(payload)
    print(f"Wrote Phase 8 writeback durability JSON to {output_path}", file=sys.stderr)

    if temp_manager is not None:
        temp_manager.cleanup()
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
