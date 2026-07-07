#!/usr/bin/env python3
"""
Replay a minimal filesystem workload against a temporary AgentFS mount.

Supported normalized JSONL/TSV operations:
  mkdir      path
  write_file path content
  read_file path
  stat      path

Supported strace-like subset:
  mkdir("path", ...)
  mkdirat(AT_FDCWD, "path", ...)
  stat/lstat/access/newfstatat-style calls with a quoted path
  open/openat/creat + write(...) + close(...) for write_file
  open/openat + read(...) for read_file

Unsupported operations are summarized and skipped. Use --dry-run to parse and
summarize without creating an AgentFS database or mount.
"""

from __future__ import annotations

import argparse
import ast
import base64
import collections
import dataclasses
import json
import os
import posixpath
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from typing import Dict, Iterable, List, Optional, Sequence


SKIP_CODE = 77
SUPPORTED_OPS = ("mkdir", "write_file", "read_file", "stat")
OP_ALIASES = {
    "mkdir": "mkdir",
    "mkdir_p": "mkdir",
    "write": "write_file",
    "write_file": "write_file",
    "writefile": "write_file",
    "read": "read_file",
    "read_file": "read_file",
    "readfile": "read_file",
    "cat": "read_file",
    "stat": "stat",
    "lstat": "stat",
    "access": "stat",
}

SYSCALL_RE = re.compile(
    r"^(?:\d+\s+)?(?:\d{2}:\d{2}:\d{2}(?:\.\d+)?\s+)?"
    r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)\((?P<args>.*)\)\s+=\s+(?P<ret>.+)$"
)
QUOTED_RE = re.compile(r'"(?:\\.|[^"\\])*"')
FD_RE = re.compile(r"\s*(-?\d+)")

STRACE_STAT_SYSCALLS = {
    "stat",
    "lstat",
    "access",
    "faccessat",
    "faccessat2",
    "newfstatat",
    "statx",
}
STRACE_OPEN_SYSCALLS = {"open", "openat", "openat2", "creat"}
STRACE_WRITE_SYSCALLS = {"write", "pwrite64"}
STRACE_READ_SYSCALLS = {"read", "pread64"}
STRACE_IGNORED_SYSCALLS = {
    "close",
    "fcntl",
    "fsync",
    "fdatasync",
    "getcwd",
    "chdir",
    "fchdir",
}
STRACE_UNSUPPORTED_FS_SYSCALLS = {
    "chmod",
    "fchmod",
    "fchmodat",
    "chown",
    "fchown",
    "fchownat",
    "link",
    "linkat",
    "mknod",
    "mknodat",
    "readlink",
    "readlinkat",
    "rename",
    "renameat",
    "renameat2",
    "rmdir",
    "symlink",
    "symlinkat",
    "truncate",
    "ftruncate",
    "unlink",
    "unlinkat",
    "utime",
    "utimes",
    "utimensat",
    "setxattr",
    "lsetxattr",
    "fsetxattr",
    "getxattr",
    "lgetxattr",
    "fgetxattr",
    "listxattr",
    "llistxattr",
    "flistxattr",
    "removexattr",
    "lremovexattr",
    "fremovexattr",
}


@dataclasses.dataclass
class Operation:
    op: str
    path: str
    data: bytes = b""
    append: bool = False
    line_no: int = 0
    source: str = ""


@dataclasses.dataclass
class Unsupported:
    line_no: int
    op: str
    reason: str
    source: str


@dataclasses.dataclass
class FdState:
    path: str
    writable: bool
    emit_empty_on_close: bool = False
    append: bool = False
    chunks: List[bytes] = dataclasses.field(default_factory=list)
    emitted_read: bool = False


@dataclasses.dataclass
class ParseResult:
    operations: List[Operation]
    unsupported: List[Unsupported]
    ignored_lines: int
    line_count: int


class ReplayError(Exception):
    pass


class PrerequisiteSkip(ReplayError):
    pass


def path_is_safe(workload_path: str) -> bool:
    if "\0" in workload_path:
        return False
    parts = [part for part in workload_path.replace("\\", "/").split("/") if part]
    return ".." not in parts


def normalize_op(op: object) -> Optional[str]:
    if not isinstance(op, str):
        return None
    return OP_ALIASES.get(op.strip().lower().replace("-", "_"))


def decode_tsv_field(value: str) -> str:
    try:
        return bytes(value, "utf-8").decode("unicode_escape")
    except UnicodeError:
        return value


def decode_c_string(token: str) -> str:
    try:
        value = ast.literal_eval(token)
    except (SyntaxError, ValueError):
        return token[1:-1]
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return str(value)


def quoted_strings(args: str) -> List[str]:
    return [decode_c_string(match.group(0)) for match in QUOTED_RE.finditer(args)]


def parse_ret_int(ret: str) -> Optional[int]:
    match = FD_RE.match(ret)
    if not match:
        return None
    try:
        return int(match.group(1))
    except ValueError:
        return None


def parse_fd(args: str) -> Optional[int]:
    match = FD_RE.match(args)
    if not match:
        return None
    try:
        return int(match.group(1))
    except ValueError:
        return None


def json_bytes(obj: dict) -> bytes:
    if "data_b64" in obj:
        return base64.b64decode(str(obj["data_b64"]), validate=True)
    for key in ("content", "data", "text"):
        if key in obj:
            value = obj[key]
            if isinstance(value, bytes):
                return value
            if isinstance(value, str):
                return value.encode("utf-8")
            return json.dumps(value, sort_keys=True).encode("utf-8")
    return b""


def first_path(obj: dict) -> Optional[str]:
    for key in ("path", "file", "pathname", "target", "name"):
        value = obj.get(key)
        if isinstance(value, str):
            return value
    return None


class WorkloadParser:
    def __init__(self) -> None:
        self.operations: List[Operation] = []
        self.unsupported: List[Unsupported] = []
        self.ignored_lines = 0
        self.line_count = 0
        self.fd_table: Dict[int, FdState] = {}

    def parse_file(self, path: str) -> ParseResult:
        with open(path, "r", encoding="utf-8", errors="replace") as input_file:
            for line_no, line in enumerate(input_file, start=1):
                self.line_count = line_no
                self.parse_line(line_no, line.rstrip("\n"))
        self.finish()
        return ParseResult(
            operations=self.operations,
            unsupported=self.unsupported,
            ignored_lines=self.ignored_lines,
            line_count=self.line_count,
        )

    def add_unsupported(self, line_no: int, op: str, reason: str, source: str) -> None:
        self.unsupported.append(Unsupported(line_no, op, reason, source.strip()))

    def add_op(
        self,
        line_no: int,
        op: str,
        path: str,
        source: str,
        data: bytes = b"",
        append: bool = False,
    ) -> None:
        if not path_is_safe(path):
            self.add_unsupported(line_no, op, "unsafe path", source)
            return
        self.operations.append(Operation(op, path, data, append, line_no, source.strip()))

    def parse_line(self, line_no: int, raw_line: str) -> None:
        line = raw_line.strip()
        if not line or line.startswith("#"):
            self.ignored_lines += 1
            return

        if line.startswith("{"):
            self.parse_json_line(line_no, line)
            return

        if "\t" in line:
            self.parse_tsv_line(line_no, line)
            return

        if self.parse_strace_line(line_no, line):
            return

        self.add_unsupported(line_no, "unknown", "unrecognized line format", line)

    def parse_json_line(self, line_no: int, line: str) -> None:
        try:
            obj = json.loads(line)
        except json.JSONDecodeError as exc:
            self.add_unsupported(line_no, "json", f"invalid JSON: {exc}", line)
            return

        if not isinstance(obj, dict):
            self.add_unsupported(line_no, "json", "JSONL entry is not an object", line)
            return

        op = normalize_op(obj.get("op") or obj.get("operation") or obj.get("syscall"))
        if op is None:
            self.add_unsupported(line_no, str(obj.get("op", "unknown")), "unsupported operation", line)
            return

        path = first_path(obj)
        if path is None:
            self.add_unsupported(line_no, op, "missing path", line)
            return

        data = json_bytes(obj) if op == "write_file" else b""
        self.add_op(line_no, op, path, line, data=data, append=bool(obj.get("append", False)))

    def parse_tsv_line(self, line_no: int, line: str) -> None:
        parts = line.split("\t", 2)
        if len(parts) < 2:
            self.add_unsupported(line_no, "tsv", "expected at least op and path columns", line)
            return

        op = normalize_op(parts[0])
        if op is None:
            self.add_unsupported(line_no, parts[0], "unsupported operation", line)
            return

        path = parts[1]
        data = decode_tsv_field(parts[2]).encode("utf-8") if op == "write_file" and len(parts) > 2 else b""
        self.add_op(line_no, op, path, line, data=data)

    def parse_strace_line(self, line_no: int, line: str) -> bool:
        if "<unfinished ...>" in line or "resumed>" in line:
            self.add_unsupported(line_no, "strace", "unfinished/resumed strace records are not supported", line)
            return True

        match = SYSCALL_RE.match(line)
        if not match:
            return False

        name = match.group("name")
        args = match.group("args")
        ret = match.group("ret")
        ret_int = parse_ret_int(ret)

        if ret_int is not None and ret_int < 0:
            self.ignored_lines += 1
            return True

        if name in {"mkdir", "mkdirat"}:
            strings = quoted_strings(args)
            if strings:
                self.add_op(line_no, "mkdir", strings[0], line)
            else:
                self.add_unsupported(line_no, name, "missing quoted path", line)
            return True

        if name in STRACE_STAT_SYSCALLS:
            strings = quoted_strings(args)
            if strings and strings[0]:
                self.add_op(line_no, "stat", strings[0], line)
            else:
                self.add_unsupported(line_no, name, "missing quoted path", line)
            return True

        if name == "fstat":
            fd = parse_fd(args)
            state = self.fd_table.get(fd) if fd is not None else None
            if state is None:
                self.add_unsupported(line_no, name, "fd is not mapped to a path", line)
            else:
                self.add_op(line_no, "stat", state.path, line)
            return True

        if name in STRACE_OPEN_SYSCALLS:
            self.handle_open(line_no, name, args, ret_int, line)
            return True

        if name in STRACE_WRITE_SYSCALLS:
            self.handle_write(line_no, args, ret_int, line)
            return True

        if name in STRACE_READ_SYSCALLS:
            self.handle_read(line_no, args, ret_int, line)
            return True

        if name == "close":
            self.handle_close(line_no, args, line)
            return True

        normalized = normalize_op(name)
        if normalized in {"mkdir", "read_file", "stat", "write_file"}:
            self.parse_normalized_call(line_no, normalized, args, line)
            return True

        if name in STRACE_IGNORED_SYSCALLS:
            self.ignored_lines += 1
            return True

        if name in STRACE_UNSUPPORTED_FS_SYSCALLS:
            self.add_unsupported(line_no, name, "filesystem syscall is outside the replay subset", line)
            return True

        self.ignored_lines += 1
        return True

    def parse_normalized_call(self, line_no: int, op: str, args: str, source: str) -> None:
        strings = quoted_strings(args)
        if not strings:
            self.add_unsupported(line_no, op, "missing quoted path", source)
            return
        data = strings[1].encode("utf-8") if op == "write_file" and len(strings) > 1 else b""
        self.add_op(line_no, op, strings[0], source, data=data)

    def handle_open(self, line_no: int, name: str, args: str, ret_int: Optional[int], source: str) -> None:
        if ret_int is None:
            self.add_unsupported(line_no, name, "open return value is not an fd", source)
            return

        strings = quoted_strings(args)
        if not strings:
            self.add_unsupported(line_no, name, "missing quoted path", source)
            return

        flags = args.upper()
        emit_empty_on_close = name == "creat" or any(flag in flags for flag in ("O_CREAT", "O_TRUNC"))
        writable = emit_empty_on_close or any(flag in flags for flag in ("O_WRONLY", "O_RDWR", "O_APPEND"))
        append = "O_APPEND" in flags
        self.fd_table[ret_int] = FdState(
            strings[0],
            writable=writable,
            emit_empty_on_close=emit_empty_on_close,
            append=append,
        )

    def handle_write(self, line_no: int, args: str, ret_int: Optional[int], source: str) -> None:
        if ret_int is None or ret_int <= 0:
            self.ignored_lines += 1
            return

        fd = parse_fd(args)
        state = self.fd_table.get(fd) if fd is not None else None
        if state is None:
            if fd is not None and fd <= 2:
                self.ignored_lines += 1
            else:
                self.add_unsupported(line_no, "write", "fd is not mapped to a path", source)
            return
        if not state.writable:
            self.add_unsupported(line_no, "write", "fd was not opened with write intent", source)
            return

        strings = quoted_strings(args)
        if not strings:
            self.add_unsupported(line_no, "write", "missing quoted write buffer", source)
            return

        data = strings[0].encode("utf-8")[:ret_int]
        if data:
            state.chunks.append(data)

    def handle_read(self, line_no: int, args: str, ret_int: Optional[int], source: str) -> None:
        if ret_int is None or ret_int < 0:
            self.ignored_lines += 1
            return

        fd = parse_fd(args)
        state = self.fd_table.get(fd) if fd is not None else None
        if state is None:
            if fd is not None and fd <= 2:
                self.ignored_lines += 1
            else:
                self.add_unsupported(line_no, "read", "fd is not mapped to a path", source)
            return

        if not state.emitted_read:
            self.add_op(line_no, "read_file", state.path, source)
            state.emitted_read = True

    def handle_close(self, line_no: int, args: str, source: str) -> None:
        fd = parse_fd(args)
        if fd is None:
            self.ignored_lines += 1
            return
        state = self.fd_table.pop(fd, None)
        if state is None:
            self.ignored_lines += 1
            return
        if state.writable and (state.chunks or state.emit_empty_on_close):
            self.add_op(line_no, "write_file", state.path, source, data=b"".join(state.chunks), append=state.append)

    def finish(self) -> None:
        for fd, state in sorted(self.fd_table.items()):
            if state.writable and (state.chunks or state.emit_empty_on_close):
                self.add_op(0, "write_file", state.path, f"<implicit close fd {fd}>", data=b"".join(state.chunks), append=state.append)
        self.fd_table.clear()


def print_summary(result: ParseResult) -> None:
    supported_counts = collections.Counter(op.op for op in result.operations)
    unsupported_counts = collections.Counter(item.op for item in result.unsupported)

    print(f"Input lines: {result.line_count}")
    print(f"Supported operations: {len(result.operations)}")
    for op in SUPPORTED_OPS:
        if supported_counts[op]:
            print(f"  {op}: {supported_counts[op]}")

    print(f"Unsupported operations: {len(result.unsupported)}")
    for op, count in sorted(unsupported_counts.items()):
        print(f"  {op}: {count}")

    if result.unsupported:
        print("Unsupported examples:")
        for item in result.unsupported[:10]:
            print(f"  line {item.line_no}: {item.op}: {item.reason}: {item.source}")

    print(f"Ignored lines: {result.ignored_lines}")


def resolve_agentfs(agentfs_bin: str) -> str:
    if os.sep in agentfs_bin:
        resolved = os.path.abspath(os.path.expanduser(agentfs_bin))
        if os.access(resolved, os.X_OK):
            return resolved
        raise PrerequisiteSkip(f"agentfs binary is not executable: {agentfs_bin}")

    resolved = shutil.which(agentfs_bin)
    if not resolved:
        raise PrerequisiteSkip(
            "agentfs binary not found. Build/install it first, or pass --agentfs-bin PATH "
            "(for example: cargo build --release -p agentfs-cli --bins && cp target/release/agentfs /usr/local/bin)."
        )
    return resolved


def is_mounted(path: str) -> bool:
    mountpoint = shutil.which("mountpoint")
    if mountpoint:
        return subprocess.run([mountpoint, "-q", path], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0
    return os.path.ismount(path)


def safe_rmtree_tmp(path: str, prefixes: Sequence[str]) -> None:
    if not path:
        return
    real = os.path.realpath(path)
    if any(real.startswith(prefix) for prefix in prefixes):
        shutil.rmtree(real, ignore_errors=True)
    else:
        print(f"Refusing to remove non-harness temp path: {real}", file=sys.stderr)


def unmount(path: str, log_file) -> None:
    for helper in ("fusermount3", "fusermount", "umount"):
        resolved = shutil.which(helper)
        if not resolved:
            continue
        command = [resolved, "-u", path] if helper.startswith("fusermount") else [resolved, path]
        subprocess.run(command, stdout=log_file, stderr=subprocess.STDOUT)
        return
    print("No fusermount3/fusermount/umount helper found for cleanup", file=log_file)


class AgentFSMount:
    def __init__(self, agentfs_bin: str, report_dir: Optional[str], keep_work: bool) -> None:
        self.agentfs_bin = resolve_agentfs(agentfs_bin)
        self.keep_work = keep_work
        self.report_dir = report_dir or tempfile.mkdtemp(prefix="agentfs-replay-report.", dir="/tmp")
        self.report_dir = os.path.abspath(self.report_dir)
        self.work_dir = ""
        self.mount_dir = ""
        self.mount_process: Optional[subprocess.Popen] = None

    def __enter__(self) -> "AgentFSMount":
        try:
            os.makedirs(self.report_dir, exist_ok=True)
            self.work_dir = tempfile.mkdtemp(prefix="agentfs-replay-work.", dir="/tmp")
            self.mount_dir = tempfile.mkdtemp(prefix="agentfs-replay-mnt.", dir="/tmp")
            agent_id = f"replay-{os.getpid()}-{int(time.time())}"
            db_path = os.path.join(self.work_dir, ".agentfs", f"{agent_id}.db")

            with open(os.path.join(self.report_dir, "init.log"), "w", encoding="utf-8") as log_file:
                subprocess.run([self.agentfs_bin, "init", agent_id], cwd=self.work_dir, stdout=log_file, stderr=subprocess.STDOUT, check=True)

            if not os.path.isfile(db_path):
                raise ReplayError(f"AgentFS database was not created at {db_path}; see {self.report_dir}/init.log")

            mount_log_path = os.path.join(self.report_dir, "mount.log")
            mount_log = open(mount_log_path, "w", encoding="utf-8")
            self.mount_process = subprocess.Popen(
                [self.agentfs_bin, "mount", db_path, self.mount_dir, "--foreground"],
                stdout=mount_log,
                stderr=subprocess.STDOUT,
            )
            mount_log.close()

            for _ in range(100):
                if is_mounted(self.mount_dir):
                    return self
                if self.mount_process.poll() is not None:
                    break
                time.sleep(0.1)

            raise ReplayError(f"AgentFS mount did not become ready at {self.mount_dir}; see {mount_log_path}")
        except Exception:
            self.cleanup()
            raise

    def __exit__(self, exc_type, exc, tb) -> None:
        self.cleanup()

    def cleanup(self) -> None:
        cleanup_path = os.path.join(self.report_dir, "cleanup.log")
        os.makedirs(self.report_dir, exist_ok=True)
        with open(cleanup_path, "a", encoding="utf-8") as log_file:
            if self.mount_dir and is_mounted(self.mount_dir):
                unmount(self.mount_dir, log_file)

            if self.mount_process is not None:
                if self.mount_process.poll() is None:
                    self.mount_process.terminate()
                    try:
                        self.mount_process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        self.mount_process.kill()
                else:
                    self.mount_process.wait()

        if not self.keep_work:
            safe_rmtree_tmp(self.work_dir, ("/tmp/agentfs-replay-work.",))
            safe_rmtree_tmp(self.mount_dir, ("/tmp/agentfs-replay-mnt.",))
        else:
            print(f"Kept work directory: {self.work_dir}", file=sys.stderr)
            print(f"Kept mount directory: {self.mount_dir}", file=sys.stderr)


def host_path(root: str, workload_path: str) -> str:
    if "\0" in workload_path:
        raise ReplayError(f"path contains NUL byte: {workload_path!r}")

    parts = [part for part in workload_path.replace("\\", "/").split("/") if part]
    if any(part == ".." for part in parts):
        raise ReplayError(f"path traversal is not allowed: {workload_path}")

    normalized = posixpath.normpath("/" + "/".join(parts))
    if normalized == "/":
        return os.path.abspath(root)

    candidate = os.path.abspath(os.path.join(root, normalized.lstrip("/")))
    root_abs = os.path.abspath(root)
    if os.path.commonpath([root_abs, candidate]) != root_abs:
        raise ReplayError(f"path escapes replay root: {workload_path}")
    return candidate


def replay_operations(operations: Iterable[Operation], mount_dir: str, report_dir: str) -> int:
    errors: List[str] = []
    replay_log_path = os.path.join(report_dir, "replay.log")
    replayed = 0

    with open(replay_log_path, "w", encoding="utf-8") as replay_log:
        for index, operation in enumerate(operations, start=1):
            replayed = index
            try:
                target = host_path(mount_dir, operation.path)
                if operation.op == "mkdir":
                    os.makedirs(target, exist_ok=True)
                elif operation.op == "write_file":
                    parent = os.path.dirname(target)
                    if parent:
                        os.makedirs(parent, exist_ok=True)
                    mode = "ab" if operation.append else "wb"
                    with open(target, mode) as output_file:
                        output_file.write(operation.data)
                elif operation.op == "read_file":
                    with open(target, "rb") as input_file:
                        input_file.read()
                elif operation.op == "stat":
                    os.stat(target)
                else:
                    raise ReplayError(f"internal unsupported op: {operation.op}")
                replay_log.write(f"ok {index} {operation.op} {operation.path}\n")
            except Exception as exc:  # noqa: BLE001 - harness should collect all replay failures.
                message = f"line {operation.line_no}: {operation.op} {operation.path}: {exc}"
                errors.append(message)
                replay_log.write(f"error {index} {message}\n")

    if errors:
        print(f"Replay failed for {len(errors)} supported operation(s):", file=sys.stderr)
        for message in errors[:20]:
            print(f"  {message}", file=sys.stderr)
        print(f"Replay log: {replay_log_path}", file=sys.stderr)
        return 1

    print(f"Replayed {replayed} supported operation(s).")
    print(f"Report directory: {report_dir}")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("logfile", help="JSONL, TSV, or strace-like workload log")
    parser.add_argument("--dry-run", action="store_true", help="parse and summarize only; do not create an AgentFS mount")
    parser.add_argument("--agentfs-bin", default=os.environ.get("AGENTFS_BIN", "agentfs"), help="agentfs executable for replay mode")
    parser.add_argument("--report-dir", default=os.environ.get("REPORT_DIR"), help="directory for init/mount/replay logs")
    parser.add_argument("--keep-work", action="store_true", help="keep temporary AgentFS work and mount directories after replay")
    return parser


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = build_parser().parse_args(argv)

    parser = WorkloadParser()
    result = parser.parse_file(args.logfile)
    print_summary(result)

    if args.dry_run:
        return 0

    if not result.operations:
        print("No supported operations to replay.")
        return 0

    if result.unsupported:
        print("Unsupported operations will be skipped during replay.", file=sys.stderr)

    active_mount: Optional[AgentFSMount] = None

    def handle_signal(signum, _frame) -> None:
        if active_mount is not None:
            active_mount.cleanup()
        raise SystemExit(128 + signum)

    old_int = signal.signal(signal.SIGINT, handle_signal)
    old_term = signal.signal(signal.SIGTERM, handle_signal)
    try:
        with AgentFSMount(args.agentfs_bin, args.report_dir, args.keep_work) as mount:
            active_mount = mount
            return replay_operations(result.operations, mount.mount_dir, mount.report_dir)
    except subprocess.CalledProcessError as exc:
        print(f"AgentFS command failed with status {exc.returncode}; see report logs.", file=sys.stderr)
        return 1
    except PrerequisiteSkip as exc:
        print(f"SKIP: {exc}", file=sys.stderr)
        return SKIP_CODE
    except ReplayError as exc:
        print(f"Replay failed: {exc}", file=sys.stderr)
        return 1
    finally:
        active_mount = None
        signal.signal(signal.SIGINT, old_int)
        signal.signal(signal.SIGTERM, old_term)


if __name__ == "__main__":
    raise SystemExit(main())
