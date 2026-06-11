# Implementation Notes — 2026-06-11-enosys-flush-drop-the-close-time-flush-round-trip

Spec: 2026-06-11-enosys-flush-drop-the-close-time-flush-round-trip.md
Approved: 2026-06-11
User comment: none

---

## 2026-06-11T15:10-07:00 — Scoping walk surfaced two attr-bearing replies beyond the spec's lookup/readdirplus
**Type**: deviation
**Context**: The spec planned pending-tail guards on lookup and readdirplus only. Walking every attr-carrying reply against the close→RELEASE window showed LINK's entry reply also carries the linked inode's attrs (kernel caches them for the attr TTL once no write-open exists), and a non-mutating SETATTR replies fs.getattr attrs without draining.
**Resolution**: Added `drain_pending_tail_for_attrs(ino)` before the SDK link call, and made setattr's `flush_pending_inode` unconditional (was gated on `mutated`). Both are no-ops behind the `pending_dirty_handles == 0` atomic fast path / a cheap scan. Rename was checked and needs nothing (no attrs in reply); kernel-side `inode_is_open_for_write` protection covers the fd-still-open case, so only the post-close window mattered.

## 2026-06-11T15:20-07:00 — Coherence gate cannot deterministically isolate the LOOKUP path; race loop + counter evidence instead
**Type**: tradeoff
**Context**: While a writer fd is open, the kernel refuses server-supplied sizes (`writeback_cache` + open-for-write), so an open-fd pending tail can't discriminate the guard. The true window is close→async-RELEASE, which can't be held open deterministically from userspace.
**Resolution**: flush-coherence.py runs a 120-iteration write→close→stat/scandir/link-stat/read race loop under entry-TTL-0 (forces LOOKUP per stat) with absolute size asserts (correctness must hold regardless of who wins the race), plus a required `fuse_pending_tail_drains >= 1` gate across noflush configs proving the window was actually hit. The open-fd sync_file_range scenario stays as a read-coherence regression check. Observed: guard fired under legacy flush too — the pre-existing pre-close window was real.

## 2026-06-11T15:35-07:00 — Promoted to default-on in the same change
**Type**: decision
**Context**: Spec gated promotion on green gates + no A/B regression. Noflush needs no root (unlike uring) and the eval cleared the bar on a loaded host: per-cycle −49% alone / −57% compound, repeated-read gate 3.00x→1.96x, git workload parity over 7 pairs (status delta was load noise: legacy itself spans 125-283ms), checkout improved 172→131ms.
**Resolution**: `AGENTFS_FUSE_NOFLUSH` defaults to true; kill switch is `=0`. Forced off under `AGENTFS_DRAIN_ON_RELEASE=1`. Coherence and phase8 suites re-run against the new default (only the two known-stale perf thresholds fail). The pending-tail guards are unconditional (not gated on noflush) since they also fix the pre-existing window and cost one atomic load when nothing is buffered.
