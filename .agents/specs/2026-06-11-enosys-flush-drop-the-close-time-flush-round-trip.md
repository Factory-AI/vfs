# ENOSYS-FLUSH (lever #2): eliminate the FLUSH round trip per close()

## Why this works (verified against torvalds/linux fs/fuse/file.c)
`fuse_flush()` calls `write_inode_now(inode, 1)` **before** checking `fc->no_flush`, so dirty writeback pages always arrive as synchronous FUSE_WRITEs at close — no data ever bypasses us. Replying ENOSYS to the first FLUSH latches `fc->no_flush=1` (treated as success); every later close() skips the round trip. Each open/read/close cycle drops from 2 sync RTs (OPEN+FLUSH) to 1; compound with uring this targets the repeated-read gate (currently 1.81x) at ≤1.5x.

## The one real hazard (from the state walk)
After close() but before the async RELEASE drains the per-fh `WriteBuffer` tail (<256KiB) into the SDK, a **cold-dentry LOOKUP or READDIRPLUS** returns SDK attrs without the tail and the kernel caches the stale size for 10s. (getattr/read/write/setattr/fsync/open already drain pending; lookup/readdirplus do not.) This window exists today pre-close; FLUSH removal stretches it past close, so it must be sealed first.

## Implementation (cli/src/fuse.rs, ~4 steps)
1. **Pending-tail guard on lookup + readdirplus** (always on, fixes the pre-existing window too):
   - Add `pending_dirty_handles: AtomicUsize` to `AgentFSFuse`; maintain empty↔nonempty transitions at the 3 buffer/drain call-site groups (write handler's `buffer_fuse_write`, `take_pending` in flush/release, `flush_open_file_pending_inode_except`/`flush_all_pending`), all already under the `open_files` lock.
   - In `lookup` (SDK-hit path) and per `readdirplus` entry: fast path `pending_dirty_handles == 0` → zero cost; else `has_pending_write_for_inode(ino)` → `flush_pending_inode(ino)` → refetch attrs via `fs.getattr` before replying/caching.
2. **ENOSYS in flush()**: new `noflush: bool` (env `AGENTFS_FUSE_NOFLUSH=1`, opt-in for eval; forced off when `drain_on_release`). The handler performs today's exact drain + conditional invalidation work; on success replies `ENOSYS` instead of ok (kernel latches no_flush, close() succeeds). On drain error: reply the real errno (no_flush not latched, errors still surface).
3. **Counters**: `fuse_noflush_enosys_replies`, `fuse_pending_tail_drains` (lookup/readdirplus guard hits) in sdk profiling.
4. **New validation script** `scripts/validation/flush-coherence.py`: cross-process write→close→immediate stat + `ls -l` size-coherence loop vs native (exercises exactly the sealed window), run under {legacy, uring} × {flush, noflush}.

## Eval (same A/B discipline)
- All correctness gates with `AGENTFS_FUSE_NOFLUSH=1` (metadata-mutation, durability, serialization, phase8) + new coherence script.
- A/B: repeated-read gate + base-read + read-path (8 pairs) + git workload (4 pairs), noflush off/on.
- **Compound run**: `AGENTFS_FUSE_URING=1 + AGENTFS_FUSE_NOFLUSH=1` — the headline number; target repeated-read ≤1.5x.
- Verdict in spec log; promotion to default-on (kill switch inverted) only if gates green and A/B shows no regression — noflush needs no root, so unlike uring it can default-on independently.

## Accepted trades (documented in notes)
- Tail-drain write errors after the first close surface via log/counter instead of close() errno (NFS-like contract; write-path threshold drains still report at write time).
- Crash-loss window widens by the close→RELEASE gap (µs); `destroy()` still drains all pending; SIGKILL semantics unchanged.