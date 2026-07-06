#!/bin/sh
set -eu

echo -n "TEST git init and commit in overlay... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-run-git.XXXXXX")"
SESSION_ID="run-git-$$"

cleanup() {
    rm -rf "$ROOT" "${HOME:?}/.agentfs/run/${SESSION_ID}"
}
trap cleanup EXIT INT TERM

run_agentfs() {
    if [ -n "${AGENTFS_BIN:-}" ]; then
        "$AGENTFS_BIN" "$@"
    else
        cargo run --quiet --manifest-path "$CLI_DIR/Cargo.toml" -- "$@"
    fi
}

# The user PATH may route `git` through a hook-manager shim that daemonizes
# out of test repos (library/environment.md); pin the distro binary.
mkdir -p "$ROOT/bin"
for candidate in /usr/bin/git /bin/git; do
    if [ -x "$candidate" ]; then
        ln -sf "$candidate" "$ROOT/bin/git"
        break
    fi
done
[ -e "$ROOT/bin/git" ] || ln -sf "$(command -v git)" "$ROOT/bin/git"
PATH="$ROOT/bin:$PATH"
export PATH

# The temp root is the overlay base layer. The session delta DB lands under
# ~/.agentfs/run/<session>; pass an explicit session id so cleanup can remove
# exactly the session dir this test created (never sweep ~/.agentfs/run).
cd "$ROOT"

# Run git operations in overlay: init, add, commit.
# Identity is set explicitly: read scoping hides ~/.gitconfig inside the
# sandbox, and CI runners have no global identity anyway.
output=$(run_agentfs run --session "$SESSION_ID" /bin/bash -c '
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME="AgentFS Test" GIT_AUTHOR_EMAIL="agentfs-test@example.com"
export GIT_COMMITTER_NAME="AgentFS Test" GIT_COMMITTER_EMAIL="agentfs-test@example.com"
mkdir test-git-repo
cd test-git-repo
git init
echo "hello" > hello.txt
git add hello.txt
git commit -m "Initial commit"
git log --oneline
' 2>&1)

# Verify we got a successful commit (git log shows commit hash and message)
echo "$output" | grep -q "Initial commit" || {
    echo "FAILED"
    echo "$output"
    exit 1
}

# Verify the directory was NOT written to the host (it's in the delta layer)
if [ -d "test-git-repo" ]; then
    echo "FAILED: test-git-repo should not exist on host filesystem"
    exit 1
fi

echo "OK"
