#!/usr/bin/env sh
#
# MCP server regression (D2, M7):
#   - tools/list equals the dispatchable tool set exactly (kv_list dispatches,
#     phantom help tools rmdir/rm/unlink/copy_file are gone from help)
#   - notifications/initialized is accepted without a response (strict clients)
#   - every tools/call writes a tool_calls audit row that `agentfs timeline`
#     renders, including while the server process is still running (cross-flow)
#   - write_file preserves the mode of existing files, creates new files 0644,
#     and its data is durable after the server exits
#   - unknown tools and unknown methods get proper JSON-RPC errors
#   - --tools filters both listing and dispatch; unknown filter names are
#     rejected at startup
set -eu

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

echo -n "TEST mcp server... "

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-mcp-server.XXXXXX")"

cleanup() {
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

fail() {
    echo "FAILED: $*"
    exit 1
}

if [ -n "${AGENTFS_BIN:-}" ]; then
    BIN="$AGENTFS_BIN"
else
    cargo +nightly build --quiet --manifest-path "$CLI_DIR/Cargo.toml" \
        || fail "could not build agentfs"
    BIN="$CLI_DIR/../../target/debug/agentfs"
fi
[ -x "$BIN" ] || fail "agentfs binary not found at $BIN"

cd "$ROOT"
"$BIN" init mcp >/dev/null 2>&1 || fail "init failed"
DB="$ROOT/.agentfs/mcp.db"

# --- Help must advertise exactly the real tool surface ----------------------
"$BIN" serve mcp --help >"$ROOT/help.out" 2>&1 || fail "serve mcp --help failed"
for tool in read_file write_file readdir mkdir remove rename stat access \
    kv_get kv_set kv_delete kv_list; do
    grep -q "$tool" "$ROOT/help.out" || fail "help does not mention tool $tool"
done
for phantom in rmdir unlink copy_file; do
    grep -q "$phantom" "$ROOT/help.out" \
        && fail "help still advertises phantom tool $phantom"
done
grep -Eq '(^|[ ,])rm(,|[ ]|$)' "$ROOT/help.out" \
    && fail "help still advertises phantom tool rm"

# --- Seed an existing 0755 file through a real mount -------------------------
HAVE_FUSE=0
if [ "$(uname -s)" = "Linux" ] && [ -e /dev/fuse ]; then
    HAVE_FUSE=1
    "$BIN" exec "$DB" sh -c 'echo "original content" > mode.txt && chmod 755 mode.txt' \
        >/dev/null 2>&1 || fail "seeding mode.txt via exec failed"
    "$BIN" exec "$DB" stat -c '%a' mode.txt 2>/dev/null | tail -1 | grep -qx 755 \
        || fail "seeded mode.txt is not 0755"
else
    echo "note: FUSE unavailable; mode seeded via MCP stat checks only"
fi

# --- Main stdio JSON-RPC session ---------------------------------------------
timeout 120 python3 - "$BIN" "$DB" "$HAVE_FUSE" >"$ROOT/session.out" 2>"$ROOT/session.err" <<'EOF' \
    || fail "stdio session failed: $(tail -20 "$ROOT/session.out" "$ROOT/session.err")"
import json, subprocess, sys

BIN, DB, HAVE_FUSE = sys.argv[1], sys.argv[2], sys.argv[3] == "1"
EXPECTED_TOOLS = ["read_file", "write_file", "readdir", "mkdir", "remove",
                  "rename", "stat", "access", "kv_get", "kv_set", "kv_delete",
                  "kv_list"]

proc = subprocess.Popen([BIN, "serve", "mcp", DB],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL, text=True)

def send(msg):
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()

def rpc(id_, method, params=None):
    send({"jsonrpc": "2.0", "id": id_, "method": method, "params": params or {}})
    resp = json.loads(proc.stdout.readline())
    assert resp.get("id") == id_, f"response id mismatch: {resp}"
    return resp

def result_text(resp):
    assert "error" not in resp, f"unexpected error: {resp}"
    return resp["result"]["content"][0]["text"]

resp = rpc(1, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {}})
assert resp["result"]["protocolVersion"], resp

# Strict clients send this right after initialize; the server must accept it
# silently. The next request's response proves nothing was written for it.
send({"jsonrpc": "2.0", "method": "notifications/initialized"})

resp = rpc(2, "tools/list")
tools = [tool["name"] for tool in resp["result"]["tools"]]
assert tools == EXPECTED_TOOLS, f"tools/list mismatch: {tools}"

# Every advertised tool dispatches with minimal valid arguments.
args = {
    "write_file": {"path": "/probe.txt", "content": "probe"},
    "read_file": {"path": "/probe.txt"},
    "readdir": {"path": "/"},
    "mkdir": {"path": "/probedir"},
    "remove": {"path": "/probedir"},
    "rename": {"from": "/probe.txt", "to": "/probe2.txt"},
    "stat": {"path": "/probe2.txt"},
    "access": {"path": "/probe2.txt"},
    "kv_set": {"key": "probe", "value": {"n": 1}},
    "kv_get": {"key": "probe"},
    "kv_list": {},
    "kv_delete": {"key": "probe"},
}
assert sorted(args) == sorted(EXPECTED_TOOLS)
for i, name in enumerate(args, start=10):
    result_text(rpc(i, "tools/call", {"name": name, "arguments": args[name]}))

# write_file preserves an existing file's mode and updates its size.
if HAVE_FUSE:
    st = json.loads(result_text(rpc(30, "tools/call",
        {"name": "stat", "arguments": {"path": "/mode.txt"}})))
    assert st["mode"] == 0o100755, f"pre-write mode: {st['mode']:o}"
    result_text(rpc(31, "tools/call",
        {"name": "write_file", "arguments": {"path": "/mode.txt", "content": "overwritten"}}))
    st = json.loads(result_text(rpc(32, "tools/call",
        {"name": "stat", "arguments": {"path": "/mode.txt"}})))
    assert st["mode"] == 0o100755, f"post-write mode changed: {st['mode']:o}"
    assert st["size"] == len("overwritten"), st

# New files get the documented default mode 0644.
result_text(rpc(33, "tools/call",
    {"name": "write_file", "arguments": {"path": "/fresh.txt", "content": "new"}}))
st = json.loads(result_text(rpc(34, "tools/call",
    {"name": "stat", "arguments": {"path": "/fresh.txt"}})))
assert st["mode"] == 0o100644, f"new file mode: {st['mode']:o}"

# A failing tool call returns an error and is audited as status=error.
resp = rpc(40, "tools/call", {"name": "read_file", "arguments": {"path": "/definitely/missing"}})
assert "error" in resp, resp

# Unknown tool -> JSON-RPC invalid-params error naming the tool.
resp = rpc(41, "tools/call", {"name": "not_a_tool", "arguments": {}})
assert resp["error"]["code"] == -32602, resp
assert "not_a_tool" in resp["error"]["message"], resp

# Unknown method with an id -> method-not-found.
resp = rpc(42, "bogus/method")
assert resp["error"]["code"] == -32601, resp

# Cross-flow: while the server process is still running, the CLI timeline
# must see the write_file audit rows.
assert proc.poll() is None, "server exited early"
out = subprocess.run([BIN, "timeline", DB, "--format", "json", "--filter", "write_file"],
    capture_output=True, text=True, timeout=60)
assert out.returncode == 0, f"timeline while live failed: {out.stderr}"
rows = json.loads(out.stdout)
live = [row for row in rows if row["name"] == "write_file" and row["status"] == "success"]
assert live, f"no write_file audit rows visible while live: {rows}"

# The session keeps working after the concurrent timeline read.
result_text(rpc(50, "tools/call", {"name": "access", "arguments": {"path": "/fresh.txt"}}))

proc.stdin.close()
proc.wait(timeout=15)
assert proc.returncode == 0, f"server exit code {proc.returncode}"
print("SESSION-OK")
EOF
grep -q 'SESSION-OK' "$ROOT/session.out" || fail "stdio session did not complete"

# --- Audit rows persist and timeline options work after the server exits ----
"$BIN" timeline "$DB" --format table >"$ROOT/tl-table.out" 2>&1 \
    || fail "timeline table failed"
grep -q 'write_file' "$ROOT/tl-table.out" || fail "timeline table missing write_file"
grep -q 'error' "$ROOT/tl-table.out" || fail "timeline table missing error status"

"$BIN" timeline "$DB" --format json --limit 1 >"$ROOT/tl-limit.out" 2>&1 \
    || fail "timeline --limit failed"
"$BIN" timeline "$DB" --format json --filter kv_set --status success \
    >"$ROOT/tl-filter.out" 2>&1 || fail "timeline --filter/--status failed"
"$BIN" timeline "$DB" --format json --status error >"$ROOT/tl-error.out" 2>&1 \
    || fail "timeline --status error failed"
python3 - "$ROOT/tl-limit.out" "$ROOT/tl-filter.out" "$ROOT/tl-error.out" <<'EOF' \
    || fail "timeline JSON output malformed"
import json, sys
limited = json.load(open(sys.argv[1]))
assert len(limited) == 1, limited
for field in ("name", "status", "duration_ms", "started_at"):
    assert field in limited[0], limited[0]
filtered = json.load(open(sys.argv[2]))
assert filtered and all(r["name"] == "kv_set" and r["status"] == "success" for r in filtered)
errors = json.load(open(sys.argv[3]))
assert any(r["name"] == "not_a_tool" for r in errors), errors
assert any(r["name"] == "read_file" for r in errors), errors
EOF

# --- Durability: MCP-written data survives the server across processes ------
if [ "$HAVE_FUSE" = 1 ]; then
    "$BIN" exec "$DB" stat -c '%a %s' mode.txt 2>/dev/null | tail -1 \
        | grep -qx '755 11' || fail "mode.txt not '755 11' after server exit"
    "$BIN" exec "$DB" cat mode.txt 2>/dev/null | tail -1 | grep -qx 'overwritten' \
        || fail "mode.txt content not durable after server exit"
fi
"$BIN" fs "$DB" cat /fresh.txt >"$ROOT/fresh.out" 2>&1 \
    || fail "fs cat fresh.txt failed"
grep -qx 'new' "$ROOT/fresh.out" || fail "fresh.txt content not durable"

# --- --tools filters listing and dispatch ------------------------------------
timeout 60 python3 - "$BIN" "$DB" >"$ROOT/filter.out" 2>&1 <<'EOF' \
    || fail "--tools filter session failed: $(tail -10 "$ROOT/filter.out")"
import json, subprocess, sys
BIN, DB = sys.argv[1], sys.argv[2]
proc = subprocess.Popen([BIN, "serve", "mcp", DB, "--tools", "read_file,kv_list"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL, text=True)
def rpc(id_, method, params):
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": id_, "method": method,
                                 "params": params}) + "\n")
    proc.stdin.flush()
    return json.loads(proc.stdout.readline())
resp = rpc(1, "tools/list", {})
tools = [tool["name"] for tool in resp["result"]["tools"]]
assert tools == ["read_file", "kv_list"], tools
resp = rpc(2, "tools/call", {"name": "write_file",
    "arguments": {"path": "/filtered.txt", "content": "leak"}})
assert resp["error"]["code"] == -32602, resp
proc.stdin.close()
proc.wait(timeout=15)
assert proc.returncode == 0
print("FILTER-OK")
EOF
grep -q 'FILTER-OK' "$ROOT/filter.out" || fail "--tools filter checks did not complete"

# --- Unknown --tools names are rejected at startup ---------------------------
if "$BIN" serve mcp "$DB" --tools copy_file </dev/null \
    >"$ROOT/badfilter.out" 2>"$ROOT/badfilter.err"; then
    fail "--tools copy_file did not fail at startup"
fi
grep -q 'copy_file' "$ROOT/badfilter.err" \
    || fail "startup error does not name the unknown tool: $(cat "$ROOT/badfilter.err")"

echo "OK"
