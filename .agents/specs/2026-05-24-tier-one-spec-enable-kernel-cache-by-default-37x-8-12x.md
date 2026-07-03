# Tier One Spec: Enable kernel cache by default (37x → ~8-12x)

## Approach

The FUSE kernel cache infrastructure (TTLs, writeback, keepcache, readdirplus) is fully implemented behind feature flags. Tier one changes the defaults from off→on and hardens the invalidation correctness to make this safe. No new caching mechanisms are introduced — this is about making the existing ones the default path.

## Files to modify

### 1. `cli/src/fuse.rs` — Default TTLs and cache policy
- Change `fuse_sync_inval_enabled_from_env()`: when `AGENTFS_FUSE_SYNC_INVAL` is unset, default to `true` instead of `false`
- Change `fuse_workers_serial_from_env()`: when `AGENTFS_FUSE_WORKERS` is unset, default to non-serial (`auto` resolution) instead of `serial`
- **Effect**: With `sync_inval=1` and `workers=auto`, the existing `FuseKernelCacheConfig::from_env()` will enable: entry TTL 1s, attr TTL 1s, neg TTL 1s, writeback cache, keepcache, readdirplus auto — all without env vars

### 2. `cli/src/fuse.rs` — Invalidation audit and hardening
- Add assertions in `setattr`, `write`, `unlink`, `rmdir`, `rename`, `mkdir`, `create`, `link`, `symlink`, `mknod` that every mutation path calls `invalidate_inode_cache` or `invalidate_entry_cache` before replying
- Ensure `flush` also invalidates (it already does)
- These compile to no-ops in release but document the contract and catch regressions in debug/test

### 3. `TESTING.md` — Update gate commands and targets
- Phase 8 targets already documented; add a `--default-cache` variant that runs without env vars to validate the new defaults

### 4. `MANUAL.md` — Update env var documentation
- Mark `AGENTFS_FUSE_SYNC_INVAL` and `AGENTFS_FUSE_WORKERS` as having new defaults
- Document that TTLs/writeback/keepcache/readdirplus are now enabled by default

## Key decisions

1. **Why change defaults rather than just document env vars?** Because the target audience (coding agents running `agentfs run`) will not set these vars. The defaults must serve the common case.

2. **Why is sync_inval safe to default on?** The `notify_inval_inode` path already handles both sync and deferred modes. With non-serial workers, sync invalidation avoids the deadlock between notify and reply that serial mode has (the existing code already detects this and falls back to deferred). The remaining risk is a mutation path that forgets to call invalidation — addressed by the audit in item 2.

3. **Why auto workers?** `auto` resolves to ~25% of CPU cores with memory bounds. For a typical 8-core machine, this gives 2 worker threads — enough to overlap read dispatch without excessive context switching.

## Downstream Tier Two connection

Tier Two (HostFS passthrough for delta reads) builds on this foundation:
- With TTLs enabled, `lookup`/`getattr` for delta inodes are kernel-cached after first access
- Tier Two adds a fast path in `OverlayFS::open` for delta inodes that have an origin mapping but zero content modifications: instead of going through `AgentFS → SQLite chunks`, it returns the HostFS base file handle directly
- The check: `SELECT COUNT(*) FROM fs_data WHERE ino = ?` — if 0 rows and origin exists, the file content is identical to base, so HostFS `pread` is safe and correct
- This eliminates SQLite from the read path for copy-up'd-but-unmodified files (common in agent workflows that chmod or stat base files)

## Risks

- **Cache staleness**: If a mutation path misses invalidation, the kernel could serve stale data for up to 1s. Mitigation: the debug assertions in item 2 catch this in test; FUSE FORGET eventually expires entries; the `cache_epoch` mechanism provides a second line of defense
- **Writeback data loss on crash**: With writeback enabled, data acknowledged to userspace may not be durable until fsync. Mitigation: the `AgentFSWriteBatcher` drains on `flush`/`fsync`/`release`/`destroy`; the Phase 8 writeback durability gate validates this
- **Worker pool overhead**: `auto` workers add thread spawn overhead. Mitigation: with `sync_inval=0`, workers still default to serial

## Alternatives rejected

- **Aggressive TTLs (5-10s)**: Higher TTLs would improve benchmark numbers but risk visible staleness in interactive use. 1s is conservative and still eliminates ~90% of repeated-stat overhead in git workflows.
- **Making writeback unconditional**: Writeback trades crash-durability for throughput. Keeping it gated behind `sync_inval=1` (now default) maintains the existing safety contract: if you need durability, you fsync.

## Validation plan

After implementation, run against Phase 8 gates:

```bash
# Smoke (correctness only)
AGENTFS_FUSE_WORKERS=25% scripts/validation/phase8-validation.py --smoke --timeout 45

# Concurrent Git correctness
AGENTFS_FUSE_WORKERS=25% scripts/validation/phase8-concurrent-git-stress.py --timeout 45 --fixture-files 12 --fixture-dirs 3 --fixture-file-size-bytes 512 --edit-files 2 --append-bytes 32

# FUSE parallelism verification
AGENTFS_FUSE_WORKERS=25% scripts/validation/fuse-serialization-stress.py --timeout 60 --files 8 --file-size-bytes 2048 --threads 4 --iterations 20 --read-bytes 512

# Git workload benchmark with profiling
AGENTFS_FUSE_WORKERS=25% AGENTFS_PROFILE=1 scripts/validation/git-workload-benchmark.py --timeout 45 --fixture-files 12 --fixture-dirs 3 --fixture-file-size-bytes 512 --read-files 8 --read-bytes 512 --edit-files 2 --skip-fsck --profile

# Full policy enforcement
AGENTFS_FUSE_WORKERS=25% scripts/validation/phase8-validation.py --full --timeout 120
```

Key profiling counters to monitor: `fuse_dispatch_max_concurrent > 1`, `fuse_exclusive_fallback_count = 0`, `fuse_ttl_entry_ms = 1000`, `fuse_writeback_cache_enabled = 1`.</rewrite>
