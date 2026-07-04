#!/bin/sh
set -e

echo -n "TEST git init and commit in overlay... "

# Clean up any previous test directory
rm -rf test-git-repo

# Run git operations in overlay: init, add, commit
output=$(cargo run -- run /bin/bash -c '
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
