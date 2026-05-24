# Session Tail

## user


## assistant
# Review Result: FUSE Profiling + Coalescer

## Concrete Actions Taken

- Read the requested specs and changed files.
- Compared the changed files against `origin/main...HEAD`.
- Used `codebase_search` and targeted line inspection for profiling and FUSE coalescer paths.
- Performed a read-only review only.

## Files Written

None.

## Findings

### Blocker — `cli/src/fuse.rs:416-437`

`setattr`/ftruncate with an `fh` flushes only that file handle’s pending writes before truncating. Other open handles for the same inode can still hold stale buffered writes, then later `flush`/`release` replays them after the truncate and can regrow or overwrite the file.

**Why it matters:** Violates truncate ordering and can corrupt file contents.

**Suggested fix:** Before any size-changing truncate, flush all pending writes for the inode via `flush_pending_inode(ino)`, regardless of whether an `fh` is provided, then perform the truncate.

---

### Major — `cli/src/fuse.rs:323-329`, `cli/src/fuse.rs:1113-1136`

`getattr` reads backend stats without flushing or overlaying pending coalesced writes. After a successful buffered `write`, a subsequent stat/getattr can report stale size/mtime until flush/fsync/release.

**Why it matters:** A write acknowledged to the caller should be visible to metadata queries that require current file size.

**Suggested fix:** Flush pending writes for the inode before `getattr`, or maintain an in-memory pending attribute overlay.

---

### Major — `cli/src/fuse.rs:177-240`, `cli/src/fuse.rs:1122-1128`

The 4 MiB buffer cap is enforced only after buffering/merging the new write. A single large write or adjacent merge can temporarily exceed the cap, and `ranges_for_flush()` clones all pending data again.

**Why it matters:** The cap is not a hard memory bound and can cause large transient allocations.

**Suggested fix:** Preflight projected buffer size before merging; flush first, split, or write-through oversized writes. Consider draining ranges instead of cloning for flush.

---

### Major — `sdk/rust/src/profiling.rs:15-28`, `cli/src/fuse.rs:127-140`, `cli/src/fuse.rs:1113-1114`

Profiling records incoming FUSE write calls/bytes, but not coalesced flush batch count, flushed range count, or flushed bytes.

**Why it matters:** The profiler cannot prove whether the coalescer reduces backend `pwrite` batches, which is the key Phase 4 metric.

**Suggested fix:** Add counters such as `fuse_flush_count`, `fuse_flush_ranges`, and `fuse_flush_bytes`, recorded in `OpenFile::flush_pending`.

---

### Minor — `sdk/rust/src/connection_pool.rs:166-179`

`connection_wait_count` increments for every successful connection acquisition, including uncontended immediate acquisitions.

**Why it matters:** The name suggests contention waits, but the value is acquisition count with elapsed semaphore wait time.

**Suggested fix:** Rename to acquisition wait metrics or only increment wait count when contention actually occurs.

## Blocker Status

Blockers found: **1**.
