#!/bin/sh
set -e

echo -n "TEST git init and commit in overlay... "

# Clean up any previous test directory
rm -rf test-git-repo

# Run git operations in overlay: init, add, commit.
# Identity is set explicitly: read scoping hides ~/.gitconfig inside the
# sandbox, and CI runners have no global identity anyway.
output=$(cargo run -- run /bin/bash -c '
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
    rm -rf test-git-repo
    exit 1
fi

echo "OK"
