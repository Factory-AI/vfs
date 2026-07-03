# FUSE STATX_BLOCKS flush-invalidation patch — upstream submission package

Status: **VALIDATED, NOT YET SENT.** Everything below is what a future session
needs to pick this up and mail it to the maintainers. The only missing pieces
are the author's `Signed-off-by:` (DCO, must be added by the human author) and
the actual send.

## The problem

Under `writeback_cache`, `fuse_flush()` invalidates cached `STATX_BLOCKS` on
**every** `close(2)`, unconditionally — including read-only fds, and even when
`no_flush` is latched (the ENOSYS shortcut jumps to the same `inval_attr_out`
label). Since plain `stat(2)` requests the basic mask, which includes
`STATX_BLOCKS`, every stat-after-close forces a synchronous `FUSE_GETATTR`
round trip that `attr_timeout` was supposed to elide.

Read-mostly workloads with open/read/close/stat patterns (build systems,
`git status`-style scanners) pay one GETATTR per file per cycle regardless of
the attribute timeout. For AgentFS this is the dominant residual on the warm
read path (measured ~2.1-2.4x native; stat-only variant runs at 1.3us/cycle
while stat+open/close runs at 12.1us/cycle with 32x the GETATTR traffic —
see the variant matrix in
`.agents/specs/2026-06-12-enosys-open-*.notes.md`, 2026-07-03 entries).

## History (cite these in any discussion)

- `cf576c58b3a2` ("fuse: invalidate inode attr in writeback cache mode",
  v5.8, 2020): added the invalidation because `du` read `st_blocks == 0`
  after a buffered write — i_blocks is never maintained by the kernel under
  writeback cache, so flush-time invalidation forces a fresh GETATTR.
- `fa5eee57e33e` ("fuse: selective attribute invalidation", v5.16, Miklos):
  narrowed the invalidation from full attrs to `STATX_BLOCKS` only.
- Nobody ever made it conditional on writes actually having happened.

## The fix (17 lines)

Key insight that makes the patch small: **i_blocks can only go stale through
the page cache.** Every other write path — direct I/O, writethrough,
`copy_file_range`, `fallocate` — already invalidates at write time via
`fuse_write_update_attr()` (`FUSE_STATX_MODSIZE` includes `STATX_BLOCKS`,
see `fs/fuse/fuse_i.h`). Page-cache dirtying has exactly two entry points:

1. the iomap buffered-write branch of `fuse_cache_write_iter()` (the
   `writeback` path), and
2. `fuse_page_mkwrite()` (mmap).

So: add a `FUSE_I_BLOCKS_DIRTY` inode state bit, set it at those two sites,
and gate the `fuse_flush()` invalidation on `test_and_clear_bit()`.

Why a per-inode bit and not `file->f_mode & FMODE_WRITE`: `fuse_flush()`
calls `write_inode_now()` unconditionally, so a read-only fd's close can
write back dirty pages produced through a *different* fd. The per-inode bit
handles that correctly (the writer's dirtying set the bit; whoever flushes
first invalidates and clears); an FMODE_WRITE gate would skip it.

The motivating `du` case from cf576c58b3a2 is unaffected: a buffered write
sets the bit, so the writer's close still invalidates.

## Artifacts

- `0001-fuse-only-invalidate-STATX_BLOCKS-on-flush-if-pages-were-dirtied.patch`
  (this directory) — format-patch output of kernel commit `85a047f20045`,
  branch `fuse-blocks-dirty` in `~/src/linux` (shallow mainline clone, base
  `51512e22e` / 7.2.0-rc1). If that tree is gone, `git am` the patch file
  onto any recent mainline; it applies to the post-iomap-conversion
  `fuse_cache_write_iter()`.
- `readpath-micro.py` (this directory) — the guest micro: 32 files x 32
  iterations of stat+open/read/close (the storm), plus two correctness
  assertions (st_blocks fresh after a 1MB buffered write and after an 8KB
  mmap write through page_mkwrite).

## Validation (2026-07-03, virtme-ng 1.41, mainline 7.2.0-rc1, same tree +- patch)

Guest: `vng --cpus 8 -m 4G`, config fragment `CONFIG_FUSE_FS=y`,
`CONFIG_FUSE_IO_URING=y`, `CONFIG_FUSE_PASSTHROUGH=y`. Workload: agentfs
(FUSE, writeback_cache, attr_timeout=10s) running `readpath-micro.py` with
`AGENTFS_PROFILE=1`, GETATTR counts from the profile summary.

| metric                    | unpatched | patched |
|---------------------------|-----------|---------|
| storm cycle time          | 18.72us   | 8.51us (2.2x) |
| FUSE_GETATTR count        | 1095      | 70 (15.6x fewer) |
| st_blocks after 1MB write | 2048 ok   | 2048 ok |
| st_blocks after mmap write| 16 ok     | 16 ok   |

Repro procedure end to end:

```sh
# host, in the kernel tree
vng -b -f fuse.config                      # build (needs bc in PATH)
vng --cpus 8 -m 4G -- bash ab-guest.sh     # boot + run micro, note counts
git am 0001-fuse-*.patch                   # or checkout fuse-blocks-dirty
vng -b -f fuse.config                      # incremental rebuild (~2 min)
vng --cpus 8 -m 4G -- bash ab-guest.sh     # compare
```

where `ab-guest.sh` mounts an agentfs db in the guest's tmpfs /tmp and runs
`AGENTFS_PROFILE=1 agentfs exec g.db python3 -- readpath-micro.py`, then
greps `fuse_op_getattr_count` from the profile summary on stderr.

A reproducer independent of agentfs (if maintainers ask): any libfuse
filesystem with writeback cache enabled (e.g. `passthrough_hp --o writeback`)
plus the same stat+open/read/close loop; count GETATTRs server-side.

## checkpatch status

`./scripts/checkpatch.pl --git HEAD`: clean except
1. `Missing Signed-off-by` — intentional, the author must add their own DCO
   line (`git commit --amend -s` as the author identity), and
2. two "Unknown commit id" warnings for cf576c58b3a2 / fa5eee57e33e — false
   positives from the shallow clone (depth 1); they resolve in a full clone.

## Routing (from `./scripts/get_maintainer.pl`)

- To: Miklos Szeredi <miklos@szeredi.hu> (maintainer, FUSE FILESYSTEM CORE)
- Cc: fuse-devel@lists.linux.dev (fuse list)
- Cc: linux-kernel@vger.kernel.org
- Optionally Cc: linux-fsdevel@vger.kernel.org (VFS-adjacent attr caching)

Target tree: Miklos' fuse.git `for-next`
(git://git.kernel.org/pub/scm/linux/kernel/git/mszeredi/fuse.git). Rebase the
patch there before sending; conflicts are unlikely (the touched hunks are
stable), but the iomap write path is recent, so confirm
`fuse_cache_write_iter()` still has the `writeback` branch.

## Remaining steps to actually send (in order)

1. Decide author identity (patch currently authored as
   `ain3sh <ainesh.chatterjee@gmail.com>`; a real full name is expected for
   DCO — amend author + add matching `Signed-off-by`).
2. Rebase onto current fuse.git for-next; re-run checkpatch; re-run the vng
   A/B if the surrounding code moved.
3. Send: `git send-email --to='Miklos Szeredi <miklos@szeredi.hu>'
   --cc=fuse-devel@lists.linux.dev --cc=linux-kernel@vger.kernel.org
   0001-*.patch` (or `b4 send` if b4 is configured). Plain patch mail, no
   cover letter needed for a single patch — the changelog carries the
   argument. Expect review feedback on (a) whether a state bit vs an
   FMODE_WRITE check is preferred, and (b) whether the bit should also be
   set in the writethrough path for belt-and-braces (argue: not needed,
   fuse_write_update_attr already invalidates there).
4. Watch https://lore.kernel.org/fuse-devel/ for replies.

## Why we care (impact on this repo, for the eventual v2/benchmarks section)

With this patch, agentfs' warm read path drops toward the persistent-fd
profile (stat-only measured 1.3us/cycle vs the 12.1us stat+open/close storm)
and the edit phase loses its 2 forced GETATTRs per edit — the two remaining
threshold misses that were classified "kernel floor" in
`.agents/specs/2026-06-11-per-phase-1-5x-roadmap-*.md`.
