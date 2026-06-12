# Implementation Notes — 2026-06-12-enosys-open-eliminate-open-release-round-trips-via-kernel-no_open

Spec: 2026-06-12-enosys-open-eliminate-open-release-round-trips-via-kernel-no_open.md
Approved: 2026-06-12
User comment: none

---

## 2026-06-12T14:05-07:00 — Coherence gate surfaced a pre-existing unlink-while-open gap (not a WS9 regression)
**Type**: surprise
**Context**: The new noopen-coherence gate asserted POSIX read-back of an unlinked-but-open file; it EIO'd. Verified the legacy fh path fails identically: SDK unlink reaps the inode immediately (no nlink-0-with-open-handles deferral), and the adapter's unlink-side kernel inval drops the page-cache copy that usually masks it.
**Resolution**: Gate trimmed to assert what the system guarantees today (unlink must not wedge subsequent I/O); post-unlink read-back AND any mutation on an unlinked-open inode logged as an SDK followup (deferred inode reap or adapter-side nlink pinning) — even the close-time writeback mtime SETATTR errors today, in both modes. Out of WS9 scope — behavior is unchanged between fh and per-ino paths.
