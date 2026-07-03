#!/bin/bash
# Runs inside the vng guest: mount agentfs, run the read-path micro, report
# GETATTR counts from the profile summary.
set -e
BIN=/home/ain3sh/factory/vfs/cli/target/release/agentfs
cd /tmp
uname -r
rm -f g.db && touch g.db
AGENTFS_PROFILE=1 $BIN exec g.db python3 -- /home/ain3sh/src/guest-ab/readpath-micro.py 2>/tmp/prof.log
python3 - <<'EOF'
import json, re
best = None
for line in open('/tmp/prof.log'):
    if 'agentfs_profile_summary' in line and 'fuse_session' in line:
        m = re.search(r'\{.*\}', line)
        if m:
            best = json.loads(m.group(0))
c = (best or {}).get('counters', best) or {}
for op in ('getattr', 'open', 'flush', 'lookup'):
    n = c.get(f'fuse_op_{op}_count', 0)
    ns = c.get(f'fuse_op_{op}_nanos', 0)
    print(f"fuse_{op}: count={n} total_ms={ns/1e6:.1f}")
EOF
