#!/bin/sh
set -e

echo -n "TEST interactive bash session... "

# Run bash session in overlay: write a file and read it back
# The current directory becomes copy-on-write with the overlay sandbox
output=$(cargo run -- run /bin/bash -c '
echo "hello from agent" > hello.txt
cat hello.txt
' 2>&1)

# Verify we got the expected output
echo "$output" | grep -q "hello from agent" || {
    echo "FAILED"
    echo "$output"
    exit 1
}

# Verify the file was NOT written to the host (it's in the delta layer)
if [ -f "hello.txt" ]; then
    echo "FAILED: hello.txt should not exist on host filesystem"
    rm -f hello.txt
    exit 1
fi

echo "OK"
