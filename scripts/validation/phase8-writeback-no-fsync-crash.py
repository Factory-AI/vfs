#!/usr/bin/env python3
"""Phase 8 no-fsync writeback crash consistency gate.

Writes bytes through AgentFS without fsync, SIGKILLs the mount while the file is
still open, remounts the same DB, and requires portable integrity plus an
unchanged base tree. The written data may be absent or a prefix of the payload,
but arbitrary corrupt bytes fail the gate.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import sys
import tempfile
import time
import traceback
import uuid
from pathlib import Path
from typing import Any, Optional


def load_common() -> Any:
    common_path = Path(__file__).with_name("phase8-writeback-durability.py")
    spec = importlib.util.spec_from_file_location("phase8_writeback_durability_common", common_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"failed to load common helpers from {common_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


common = load_common()


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify no-fsync AgentFS crash leaves a remountable, portable, base-preserving DB."
    )
    parser.add_argument("--write-bytes", type=common.positive_int, default=8192)
    parser.add_argument(
        "--agentfs-bin",
        default=os.environ.get("AGENTFS_BIN"),
        help="agentfs executable path/name (default: repo target binary, building cli if needed)",
    )
    parser.add_argument(
        "--timeout",
        type=common.positive_float,
        default=common.positive_float(os.environ.get("PHASE8_WRITEBACK_TIMEOUT", "90")),
    )
    parser.add_argument("--session", default=None)
    parser.add_argument("--keep-temp", action="store_true", default=common.env_flag("PHASE8_KEEP_TEMP"))
    parser.add_argument("--output", help="write JSON result to this file")
    parser.add_argument("--json-indent", type=int, default=2)
    return parser.parse_args(argv)


def default_output_path() -> Path:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return Path(tempfile.gettempdir()) / f"agentfs-phase8-writeback-no-fsync-{stamp}-{uuid.uuid4().hex[:8]}.json"


def classify_remount_read(read_bytes: bytes, expected: bytes, read_error: Optional[str], error_kind: Optional[str]) -> dict[str, Any]:
    if read_error is None:
        prefix_ok = expected.startswith(read_bytes) and len(read_bytes) <= len(expected)
        if len(read_bytes) == len(expected) and read_bytes == expected:
            state = "present_full"
        elif prefix_ok:
            state = "present_prefix_or_empty"
        else:
            state = "corrupt_or_unexpected"
        return {
            "state": state,
            "accepted": prefix_ok,
            "error": None,
            "error_kind": None,
            "bytes": len(read_bytes),
            "sha256": hashlib.sha256(read_bytes).hexdigest(),
            "prefix_of_expected": prefix_ok,
        }
    if error_kind == "FileNotFoundError":
        return {
            "state": "missing",
            "accepted": True,
            "error": read_error,
            "error_kind": error_kind,
            "bytes": 0,
            "sha256": None,
            "prefix_of_expected": True,
        }
    return {
        "state": "read_error",
        "accepted": False,
        "error": read_error,
        "error_kind": error_kind,
        "bytes": 0,
        "sha256": None,
        "prefix_of_expected": False,
    }


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    output_path = Path(args.output).expanduser() if args.output else default_output_path()

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        temp_root = Path(tempfile.mkdtemp(prefix="agentfs-phase8-writeback-no-fsync-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(
            prefix="agentfs-phase8-writeback-no-fsync-",
            ignore_cleanup_errors=True,
        )
        temp_root = Path(temp_manager.name)

    mount_proc = None
    remount_proc = None
    mountpoint: Optional[Path] = None
    exit_code = 0
    result: dict[str, Any]
    try:
        agentfs_bin = common.resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env = common.prepare_environment(temp_root)
        session = args.session or f"phase8-no-fsync-{uuid.uuid4().hex}"
        base_root = temp_root / "base"
        common.create_base_fixture(base_root)
        base_before = common.tree_hash(base_root)
        db_path = temp_root / ".agentfs" / f"{session}.db"

        init_run = common.run_subprocess(
            [agentfs_bin, "init", "--force", "--base", str(base_root), session],
            temp_root,
            env,
            args.timeout,
        )
        if init_run["returncode"] != 0:
            raise RuntimeError(f"agentfs init failed: {init_run['stderr_tail']}")

        mountpoint = temp_root / "mnt"
        mountpoint.mkdir(parents=True, exist_ok=True)
        mount_proc, mount_start = common.start_mount(agentfs_bin, session, mountpoint, env, args.timeout)
        expected = common.deterministic_bytes(args.write_bytes)
        write_path = mountpoint / "no_fsync_crash.bin"
        started_write = time.perf_counter()
        handle = write_path.open("wb", buffering=0)
        write_error = None
        written = 0
        try:
            written = handle.write(expected)
        except Exception as exc:
            write_error = str(exc)
        sent_sigkill = False
        if mount_proc.poll() is None:
            os.killpg(mount_proc.pid, common.signal.SIGKILL)
            sent_sigkill = True
        try:
            mount_proc.wait(timeout=10)
        except Exception:
            mount_proc.kill()
        kill_process_output = common.collect_process(mount_proc)
        mount_proc = None
        close_error = None
        try:
            handle.close()
        except Exception as exc:
            close_error = str(exc)
        unmount_attempts = common.unmount(mountpoint)
        kill_record = {
            "sent_sigkill": sent_sigkill,
            "process": kill_process_output,
            "unmount_attempts": unmount_attempts,
            "mounted_after": common.is_mountpoint(mountpoint),
        }
        write_record = {
            "path": str(write_path),
            "bytes_requested": len(expected),
            "bytes_write_returned": written,
            "write_error": write_error,
            "close_after_kill_error": close_error,
            "duration_seconds": time.perf_counter() - started_write,
            "sha256": hashlib.sha256(expected).hexdigest(),
            "fsync_called": False,
        }

        remount_proc, remount_start = common.start_mount(agentfs_bin, session, mountpoint, env, args.timeout)
        read_error = None
        error_kind = None
        read_bytes = b""
        try:
            read_bytes = write_path.read_bytes()
        except Exception as exc:
            read_error = str(exc)
            error_kind = type(exc).__name__
        remount_read = classify_remount_read(read_bytes, expected, read_error, error_kind)
        clean_unmount = common.stop_mount_clean(remount_proc, mountpoint)
        remount_proc = None

        integrity = common.run_integrity(agentfs_bin, db_path, temp_root, env, args.timeout)
        db_inspect = common.inspect_db(db_path)
        sidecars = common.sidecar_status(db_path)
        base_after = common.tree_hash(base_root)
        base_unchanged = base_before["sha256"] == base_after["sha256"]

        passed = (
            init_run["returncode"] == 0
            and write_error is None
            and kill_record.get("sent_sigkill") is True
            and remount_read["accepted"] is True
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
            "benchmark": "phase8-writeback-no-fsync-crash",
            "git_commit": common.git_commit(repo_root),
            "parameters": {"write_bytes": args.write_bytes, "timeout_seconds": args.timeout},
            "agentfs": {"bin": agentfs_bin, "session": session, "db_path": str(db_path)},
            "summary": {
                "passed": passed,
                "data_state_after_remount": remount_read["state"],
                "data_after_remount_accepted": remount_read["accepted"],
                "sent_sigkill": kill_record.get("sent_sigkill"),
                "integrity_ok": integrity.get("ok"),
                "base_unchanged": base_unchanged,
                "strict_no_sidecar_files": sidecars["strict_no_sidecar_files"],
            },
            "runs": {
                "init": init_run,
                "mount": mount_start,
                "write_without_fsync": write_record,
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
            "benchmark": "phase8-writeback-no-fsync-crash",
            "error": str(exc),
            "traceback": traceback.format_exc(),
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }
    finally:
        for proc in (mount_proc, remount_proc):
            if proc is not None and proc.poll() is None:
                common.terminate_process_tree(proc)
        if mountpoint is not None:
            try:
                common.unmount(mountpoint)
            except Exception:
                pass

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(payload, encoding="utf-8")
    sys.stdout.write(payload)
    print(f"Wrote Phase 8 no-fsync crash JSON to {output_path}", file=sys.stderr)

    if temp_manager is not None:
        temp_manager.cleanup()
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
