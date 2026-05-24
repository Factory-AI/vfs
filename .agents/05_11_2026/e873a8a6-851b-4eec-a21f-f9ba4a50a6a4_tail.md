# Session Tail

## user


## assistant
SUMMARY:
Implemented/finished Phase 8 dispatch in the dispatch worktree: owned FUSE requests, shared `SessionShared` state, `AGENTFS_FUSE_WORKERS` / `AGENTFS_FUSE_QUEUE` resolution, serial rollback mode, bounded std-thread worker dispatch with inline fallback, and `&self` local fuser callbacks. Verified profiling dispatch counters are exported and wired.

FILES CHANGED:
- cli/src/fuse.rs
- cli/src/fuser/mod.rs
- cli/src/fuser/request.rs
- cli/src/fuser/session.rs
- cli/src/mount/fuse.rs
- sdk/rust/src/profiling.rs

CARGO CHECK: pass
```text
Compiling agentfs v0.6.4 (/home/ain3sh/factory/vfs-phase8-worktrees/dispatch/cli)
Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.81s
```

CARGO TEST LIB: pass
```text
Finished `test` profile [unoptimized + debuginfo] target(s) in 1m 32s
Running unittests src/lib.rs
cargo test: 106 passed (1 suite, 0.33s)
```

NOTES FOR INTEGRATION:
Serial mode is available with `AGENTFS_FUSE_WORKERS=serial`; default worker count uses logical CPUs clamped to 8 and queue defaults to `4 * workers`. The dispatch worker queue uses `std::sync::mpsc::sync_channel`; full/disconnected queues fall back to inline dispatch rather than dropping requests.
