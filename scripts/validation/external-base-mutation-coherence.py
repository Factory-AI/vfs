#!/usr/bin/env python3
"""Validate immediate overlay base data, metadata, and namespace coherence."""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib.common import resolve_vfs_bin, tail_text  # noqa: E402


DEFAULT_CLEANUP_TIMEOUT = 10.0
FINAL_UNMOUNT_SETTLE_TIMEOUT = 0.5
METADATA_PROBE_COUNT = 64
MAX_METADATA_CALLBACKS = 12
STALE_BYTES = b"before-base-content\n"
FRESH_BYTES = b"after-base-content-with-new-size\n"
MOUNTINFO_ESCAPE_RE = re.compile(r"\\([0-7]{3})")
EIO_MARKERS = ("Input/output error", "EIO")


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def parse_profile_summary(output: str) -> dict[str, Any]:
    for line in reversed(output.splitlines()):
        if '"vfs_profile_summary"' not in line:
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


def is_eio_failure(proc: subprocess.CompletedProcess[Any]) -> bool:
    if proc.returncode == 0:
        return False
    stderr = proc.stderr
    if isinstance(stderr, bytes):
        stderr_text = stderr.decode("utf-8", "replace")
    else:
        stderr_text = stderr or ""
    return any(marker in stderr_text for marker in EIO_MARKERS)


def decode_mountinfo_field(value: str) -> str:
    return MOUNTINFO_ESCAPE_RE.sub(lambda match: chr(int(match.group(1), 8)), value)


def mountinfo_entry(mountpoint: Path) -> dict[str, str] | None:
    target = str(mountpoint)
    try:
        lines = Path("/proc/self/mountinfo").read_text().splitlines()
    except OSError:
        return None

    for line in lines:
        parts = line.split()
        if len(parts) < 10:
            continue
        try:
            separator = parts.index("-")
        except ValueError:
            continue
        mounted_at = decode_mountinfo_field(parts[4])
        if mounted_at != target:
            continue
        return {
            "mountpoint": mounted_at,
            "device": parts[2],
            "fstype": parts[separator + 1] if separator + 1 < len(parts) else "",
            "source": parts[separator + 2] if separator + 2 < len(parts) else "",
            "line": line,
        }
    return None


def is_mounted(mountpoint: Path) -> bool:
    return mountinfo_entry(mountpoint) is not None


def wait_for_mount(mountpoint: Path, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if is_mounted(mountpoint):
            return True
        time.sleep(0.1)
    return False


def wait_for_unmount(mountpoint: Path, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not is_mounted(mountpoint):
            return True
        time.sleep(0.1)
    return not is_mounted(mountpoint)


def fuse_connection_id_for_mount(mountpoint: Path) -> str | None:
    entry = mountinfo_entry(mountpoint)
    if entry is None or not entry["fstype"].startswith("fuse"):
        return None
    major_minor = entry["device"].split(":", 1)
    if len(major_minor) != 2 or major_minor[0] != "0":
        return None
    return major_minor[1]


def abort_fuse_connection(mountpoint: Path) -> dict[str, Any]:
    connection_id = fuse_connection_id_for_mount(mountpoint)
    report: dict[str, Any] = {
        "attempted": connection_id is not None,
        "connection_id": connection_id,
        "ok": False,
    }
    if connection_id is None:
        report["reason"] = "no-fuse-connection-for-mountpoint"
        return report

    abort_path = Path("/sys/fs/fuse/connections") / connection_id / "abort"
    try:
        abort_path.write_text("1\n")
    except FileNotFoundError:
        report["reason"] = "connection-already-gone"
        report["ok"] = True
    except OSError as exc:
        report["reason"] = f"{type(exc).__name__}: {exc}"
    else:
        report["ok"] = True
        report["reason"] = "aborted"
    return report


def run_cleanup_command(command: list[str], timeout: float, mountpoint: Path | None = None) -> dict[str, Any]:
    report: dict[str, Any] = {"command": command, "timed_out": False}
    try:
        proc = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except FileNotFoundError:
        report.update({"returncode": None, "missing": True})
        return report

    report["pid"] = proc.pid
    try:
        report["returncode"] = proc.wait(timeout=timeout)
        return report
    except subprocess.TimeoutExpired:
        report["timed_out"] = True
        if mountpoint is not None:
            report["abort_fallback"] = abort_fuse_connection(mountpoint)
        for action in ("terminate", "kill"):
            if proc.poll() is not None:
                break
            try:
                getattr(proc, action)()
            except ProcessLookupError:
                break
            try:
                proc.wait(timeout=1.0)
            except subprocess.TimeoutExpired:
                continue
        report["returncode"] = proc.poll()
        report["alive_after_kill"] = proc.poll() is None
        return report


def unmount(mountpoint: Path, cleanup_timeout: float = DEFAULT_CLEANUP_TIMEOUT) -> dict[str, Any]:
    report: dict[str, Any] = {
        "mountpoint": str(mountpoint),
        "mounted_before": is_mounted(mountpoint),
        "connection_id_before": fuse_connection_id_for_mount(mountpoint),
        "commands": [],
        "abort_fallbacks": [],
    }
    if not report["mounted_before"]:
        report["mounted_after"] = False
        return report

    for command in (["fusermount3", "-u", str(mountpoint)], ["fusermount", "-u", str(mountpoint)]):
        command_report = run_cleanup_command(command, cleanup_timeout, mountpoint)
        report["commands"].append(command_report)
        if wait_for_unmount(mountpoint, 1.0):
            report["mounted_after"] = False
            return report

    abort_report = abort_fuse_connection(mountpoint)
    report["abort_fallbacks"].append(abort_report)
    if abort_report.get("ok"):
        wait_for_unmount(mountpoint, cleanup_timeout)
        if is_mounted(mountpoint):
            retry = run_cleanup_command(["fusermount3", "-u", str(mountpoint)], cleanup_timeout, mountpoint)
            report["commands"].append(retry)

    wait_for_unmount(mountpoint, FINAL_UNMOUNT_SETTLE_TIMEOUT)
    report["mounted_after"] = is_mounted(mountpoint)
    return report


def terminate_mount_process(
    proc: subprocess.Popen[str] | None,
    mountpoint: Path,
    cleanup_timeout: float = DEFAULT_CLEANUP_TIMEOUT,
) -> dict[str, Any]:
    report: dict[str, Any] = {"started": proc is not None}
    if proc is None:
        return report
    report["pid"] = proc.pid
    if proc.poll() is not None:
        report["returncode"] = proc.returncode
        return report

    try:
        proc.terminate()
    except ProcessLookupError:
        report["returncode"] = proc.poll()
        return report
    try:
        report["returncode"] = proc.wait(timeout=cleanup_timeout)
        report["terminated"] = True
        return report
    except subprocess.TimeoutExpired:
        report["terminated"] = False
        report["abort_fallback"] = abort_fuse_connection(mountpoint)

    if proc.poll() is None:
        try:
            proc.kill()
        except ProcessLookupError:
            pass
        try:
            proc.wait(timeout=1.0)
        except subprocess.TimeoutExpired:
            pass
    report["returncode"] = proc.poll()
    report["alive_after_kill"] = proc.poll() is None
    return report


@contextlib.contextmanager
def defer_cleanup_interrupts() -> Any:
    deferred: list[int] = []
    previous_handlers: dict[int, Any] = {}

    def record_signal(signum: int, _frame: Any) -> None:
        deferred.append(signum)

    for signum in (signal.SIGINT, signal.SIGTERM):
        previous_handlers[signum] = signal.getsignal(signum)
        signal.signal(signum, record_signal)
    try:
        yield deferred
    finally:
        for signum, handler in previous_handlers.items():
            signal.signal(signum, handler)


def cleanup_mount(
    mountpoint: Path,
    proc: subprocess.Popen[str] | None,
    cleanup_timeout: float = DEFAULT_CLEANUP_TIMEOUT,
) -> dict[str, Any]:
    with defer_cleanup_interrupts() as deferred_signals:
        report = {
            "unmount": unmount(mountpoint, cleanup_timeout),
            "process": terminate_mount_process(proc, mountpoint, cleanup_timeout),
            "mounted_after_cleanup": is_mounted(mountpoint),
        }
    if deferred_signals:
        report["deferred_signals"] = deferred_signals
    return report


def raise_keyboard_interrupt(signum: int, _frame: Any) -> None:
    raise KeyboardInterrupt(f"signal {signum}")


def run_leg(
    label: str,
    extra_env: dict[str, str],
    expect_noopen: bool,
    vfs_bin: str,
    timeout: float,
    cleanup_timeout: float,
) -> dict[str, Any]:
    started = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix=f"vfs-base-drift-{label}-") as tmp:
        root = Path(tmp)
        work = root / "work"
        base = root / "base"
        mountpoint = root / "mnt"
        work.mkdir()
        base.mkdir()
        mountpoint.mkdir()
        base_file = base / "base.txt"
        created_file = base / "created.txt"
        deleted_file = base / "deleted.txt"
        base_file.write_bytes(STALE_BYTES)
        deleted_file.write_bytes(STALE_BYTES)

        env = os.environ.copy()
        env.update(extra_env)
        env["VFS_PROFILE"] = "1"

        init = run_checked([vfs_bin, "init", "drift", "--base", str(base)], work, env, timeout)
        if init.returncode != 0:
            return {
                "label": label,
                "passed": False,
                "phase": "init",
                "returncode": init.returncode,
                "stderr_tail": tail_text(init.stderr),
            }

        mount_log = root / "mount.log"
        mount_output = ""
        mount_out = mount_log.open("w", encoding="utf-8")
        proc: subprocess.Popen[str] | None = None
        result: dict[str, Any] | None = None
        try:
            proc = subprocess.Popen(
                [
                    vfs_bin,
                    "mount",
                    str(work / ".vfs" / "drift.db"),
                    str(mountpoint),
                    "--foreground",
                ],
                cwd=str(work),
                env=env,
                stdout=mount_out,
                stderr=subprocess.STDOUT,
                text=True,
            )
            if not wait_for_mount(mountpoint, min(timeout, 20.0)):
                result = {
                    "label": label,
                    "passed": False,
                    "phase": "mount",
                    "returncode": proc.poll(),
                }
                return result

            first = subprocess.run(
                ["cat", str(mountpoint / "base.txt")],
                text=False,
                capture_output=True,
                timeout=timeout,
            )
            if first.returncode != 0 or first.stdout != STALE_BYTES:
                result = {
                    "label": label,
                    "passed": False,
                    "phase": "prime_read",
                    "returncode": first.returncode,
                    "stdout": first.stdout.decode("utf-8", "replace"),
                    "stderr_tail": tail_text(first.stderr.decode("utf-8", "replace")),
                }
                return result

            for _ in range(METADATA_PROBE_COUNT):
                try:
                    probe_size = (mountpoint / "base.txt").stat().st_size
                except OSError as exc:
                    result = {
                        "label": label,
                        "passed": False,
                        "phase": "metadata_cache_probe",
                        "error": f"{type(exc).__name__}: {exc}",
                    }
                    return result
                if probe_size != len(STALE_BYTES):
                    result = {
                        "label": label,
                        "passed": False,
                        "phase": "metadata_cache_probe",
                        "size": probe_size,
                    }
                    return result

            mounted_created = mountpoint / created_file.name
            mounted_deleted = mountpoint / deleted_file.name
            try:
                mounted_deleted_size = mounted_deleted.stat().st_size
            except OSError as exc:
                result = {
                    "label": label,
                    "passed": False,
                    "phase": "positive_entry_prime",
                    "error": f"{type(exc).__name__}: {exc}",
                }
                return result
            if mounted_deleted_size != len(STALE_BYTES):
                result = {
                    "label": label,
                    "passed": False,
                    "phase": "positive_entry_prime",
                    "size": mounted_deleted_size,
                }
                return result
            try:
                mounted_created.stat()
            except FileNotFoundError:
                pass
            except OSError as exc:
                result = {
                    "label": label,
                    "passed": False,
                    "phase": "negative_entry_prime",
                    "error": f"{type(exc).__name__}: {exc}",
                }
                return result
            else:
                result = {
                    "label": label,
                    "passed": False,
                    "phase": "negative_entry_prime",
                    "error": "created.txt unexpectedly exists before host-side creation",
                }
                return result

            before_stat = base_file.stat()
            before_hash = sha256(base_file.read_bytes())

            base_file.write_bytes(FRESH_BYTES)
            created_file.write_bytes(FRESH_BYTES)
            deleted_file.unlink()
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
            try:
                created_after_size = mounted_created.stat().st_size
            except OSError:
                created_after_size = None
            try:
                mounted_deleted.stat()
            except FileNotFoundError:
                deleted_after_missing = True
            except OSError:
                deleted_after_missing = False
            else:
                deleted_after_missing = False
        except KeyboardInterrupt as exc:
            result = {
                "label": label,
                "passed": False,
                "phase": "interrupted",
                "interrupted": True,
                "duration_seconds": time.perf_counter() - started,
                "interrupt": str(exc) or "keyboard interrupt",
            }
            return result
        finally:
            mount_out.close()
            cleanup = cleanup_mount(mountpoint, proc, cleanup_timeout)
            if mount_log.exists():
                mount_output = mount_log.read_text(encoding="utf-8", errors="replace")
            if result is not None:
                result["cleanup"] = cleanup
                result.setdefault("mount_output_tail", tail_text(mount_output))

        counters = parse_profile_summary(mount_output)
        stale = read_after.returncode == 0 and read_after.stdout == STALE_BYTES
        fresh = read_after.returncode == 0 and read_after.stdout == FRESH_BYTES
        drift_error = is_eio_failure(read_after) or is_eio_failure(stat_after)
        stat_size = stat_after.stdout.strip().split(" ")[0] if stat_after.returncode == 0 else None
        stat_fresh = stat_size == str(len(FRESH_BYTES))
        created_after_fresh = created_after_size == len(FRESH_BYTES)
        namespace_fresh = created_after_fresh and deleted_after_missing
        noopen_ok = True
        if expect_noopen:
            noopen_ok = counters.get("fuse_noopen_enosys_replies") == 1 and counters.get("fuse_op_open_count") == 1
        else:
            noopen_ok = (counters.get("fuse_op_open_count") or 0) >= 1 and (
                counters.get("fuse_noopen_enosys_replies") or 0
            ) == 0
        invalidations = counters.get("base_fast_inode_invalidations") or 0
        stale_rejections = counters.get("base_fast_stale_rejections") or 0
        lookup_count = counters.get("fuse_lookup_count") or 0
        getattr_count = counters.get("fuse_getattr_count") or 0
        metadata_counters_present = "fuse_lookup_count" in counters and "fuse_getattr_count" in counters
        metadata_cache_ok = (
            metadata_counters_present
            and 0 < lookup_count <= MAX_METADATA_CALLBACKS
            and 0 < getattr_count <= MAX_METADATA_CALLBACKS
        )
        coherence_ok = ((fresh and stat_fresh) or drift_error) and not stale and namespace_fresh

        return {
            "label": label,
            "passed": coherence_ok and noopen_ok and metadata_cache_ok,
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
            "drift_errno": "EIO" if drift_error else None,
            "stat_fresh": stat_fresh,
            "created_after_size": created_after_size,
            "created_after_fresh": created_after_fresh,
            "deleted_after_missing": deleted_after_missing,
            "namespace_fresh": namespace_fresh,
            "noopen_ok": noopen_ok,
            "fuse_op_open_count": counters.get("fuse_op_open_count"),
            "fuse_noopen_enosys_replies": counters.get("fuse_noopen_enosys_replies"),
            "fuse_lookup_count": lookup_count,
            "fuse_getattr_count": getattr_count,
            "metadata_probe_count": METADATA_PROBE_COUNT,
            "max_metadata_callbacks": MAX_METADATA_CALLBACKS,
            "metadata_counters_present": metadata_counters_present,
            "metadata_cache_ok": metadata_cache_ok,
            "base_fast_inode_invalidations": invalidations,
            "base_fast_stale_rejections": stale_rejections,
            "profile_counters_present": bool(counters),
            "mount_output_tail": tail_text(mount_output),
            "cleanup": cleanup,
        }


def main() -> int:
    signal.signal(signal.SIGINT, raise_keyboard_interrupt)
    signal.signal(signal.SIGTERM, raise_keyboard_interrupt)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vfs-bin", default=os.environ.get("VFS_BIN"))
    parser.add_argument("--timeout", type=float, default=60.0)
    parser.add_argument("--cleanup-timeout", type=float, default=DEFAULT_CLEANUP_TIMEOUT)
    parser.add_argument("--output", default=None)
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parents[2]
    vfs_bin = resolve_vfs_bin(args.vfs_bin, repo_root)
    legs = [
        ("default_noopen", {}, True),
        ("noopen_off", {"VFS_FUSE_NOOPEN": "0"}, False),
    ]
    runs = []
    interrupted = False
    for label, env, noopen in legs:
        leg_started = time.perf_counter()
        try:
            run = run_leg(label, env, noopen, vfs_bin, args.timeout, args.cleanup_timeout)
        except KeyboardInterrupt as exc:
            run = {
                "label": label,
                "passed": False,
                "phase": "interrupted",
                "interrupted": True,
                "duration_seconds": time.perf_counter() - leg_started,
                "interrupt": str(exc) or "keyboard interrupt",
            }
        runs.append(run)
        if run.get("interrupted"):
            interrupted = True
            break
    report = {
        "schema_version": 1,
        "vfs_bin": vfs_bin,
        "passed": not interrupted and all(run.get("passed") for run in runs),
        "interrupted": interrupted,
        "runs": runs,
    }
    output = args.output or os.path.join(
        tempfile.gettempdir(),
        f"vfs-external-base-mutation-{time.strftime('%Y%m%d-%H%M%S')}.json",
    )
    Path(output).write_text(json.dumps(report, indent=2))

    for run in runs:
        if run.get("interrupted") or run.get("phase") == "interrupted" or run.get("run_phase") == "interrupted":
            print(
                f"INTERRUPTED {run['label']:15s} "
                f"duration={run.get('duration_seconds')} interrupt={run.get('interrupt')!r}"
            )
            continue
        status = "PASS" if run.get("passed") else "FAIL"
        print(
            f"{status} {run['label']:15s} stale={run.get('stale')} "
            f"fresh={run.get('fresh')} stat_fresh={run.get('stat_fresh')} "
            f"namespace_fresh={run.get('namespace_fresh')} "
            f"read_rc={run.get('mounted_read_returncode')} opens={run.get('fuse_op_open_count')} "
            f"enosys={run.get('fuse_noopen_enosys_replies')} "
            f"lookup={run.get('fuse_lookup_count')} getattr={run.get('fuse_getattr_count')} "
            f"metadata_cache={run.get('metadata_cache_ok')} "
            f"base_invalidations={run.get('base_fast_inode_invalidations')} "
            f"stale_rejections={run.get('base_fast_stale_rejections')}"
        )
        if not run.get("passed"):
            print(f"  mounted_read={run.get('mounted_read')!r}")
            print(f"  mounted_stat={run.get('mounted_stat')!r}")
    print(f"report: {output}")
    if interrupted:
        return 130
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
