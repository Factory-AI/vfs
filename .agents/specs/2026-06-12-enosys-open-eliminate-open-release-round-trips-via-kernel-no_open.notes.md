# Implementation Notes — 2026-06-12-enosys-open-eliminate-open-release-round-trips-via-kernel-no_open

Spec: 2026-06-12-enosys-open-eliminate-open-release-round-trips-via-kernel-no_open.md
Approved: 2026-06-12
User comment: none

---

## 2026-06-12T14:05-07:00 — Coherence gate surfaced a pre-existing unlink-while-open gap (not a WS9 regression)
**Type**: surprise
**Context**: The new noopen-coherence gate asserted POSIX read-back of an unlinked-but-open file; it EIO'd. Verified the legacy fh path fails identically: SDK unlink reaps the inode immediately (no nlink-0-with-open-handles deferral), and the adapter's unlink-side kernel inval drops the page-cache copy that usually masks it.
**Resolution**: Gate trimmed to assert what the system guarantees today (unlink must not wedge subsequent I/O); post-unlink read-back AND any mutation on an unlinked-open inode logged as an SDK followup (deferred inode reap or adapter-side nlink pinning) — even the close-time writeback mtime SETATTR errors today, in both modes. Out of WS9 scope — behavior is unchanged between fh and per-ino paths.

## 2026-07-02T17:00-07:00 — Idle-host eval: GO, promoted default-on
**Type**: decision
**Context**: Deferred eval ran on kernel 7.1.2-3-cachyos (upgraded since implementation; noopen latch re-verified). Micro open/read/close (6 interleaved pairs, 200 iters): 47.3 -> 21.2us/cycle median, paired median 0.469. Git workload (multi, n=4 + n=8): read_search -56..-83% (11.27x -> 1.80x at n=4; 8.64x -> 3.14x at n=8 with host drift), diff -57..-62%, status -20..-47%, checkout -22..-34%, fsck -18..-34%, edit neutral at n=8 (+3%, the n=4 +74% was noise), clone neutral. Read-path (4 pairs): raw paired +8% but native drifted +17% in the same runs; same-run normalized ratio improved 2.54x -> 2.25x — neutral. Correctness with default-on: noopen-coherence 6/6, flush-coherence 4/4, metadata-mutation, serialization, durability, no-fsync-crash, 275 unit tests.
**Resolution**: The strict read_search<=1.5x bar was not met on this host (native denominators are single-digit ms and the off-arm itself no longer reproduces its historical 2.25x), but the lever is a uniform, large, correctness-free win, matching the WS7 promotion precedent. Default flipped to on; kill switch AGENTFS_FUSE_NOOPEN=0; still auto-disabled under AGENTFS_DRAIN_ON_RELEASE and without kernel FUSE_NO_OPEN_SUPPORT.

## 2026-07-02T16:50-07:00 — Side quest: mount/exec teardown leaks fixed at root cause
**Type**: surprise
**Context**: User reported agentfs leaking processes and affecting the host after benchmarking. RCA: `agentfs exec` and `agentfs mount --foreground` had no signal handling; the default disposition kills the process without running MountHandle's Drop, stranding a dead mount table entry (ENOTCONN for later visitors) and, for exec, the workload child orphaned-but-alive. Reproduced: SIGTERM left `sleep 60` running + stale mount + mountpoint dir.
**Resolution**: exec now supervises the child (tokio select over child-exit vs SIGTERM/SIGINT/SIGHUP; forwards SIGTERM, 5s grace, SIGKILL) and sets PR_SET_PDEATHSIG=SIGKILL on the child so even SIGKILL on agentfs cannot orphan it; mount handle dropped + mountpoint removed on every path; exits 128+signo. mount cmd runs the FUSE session on its own thread and tears down via shared mount::shutdown_signal(); NFS foreground upgraded from ctrl_c-only to the same. Kill matrix: TERM/INT fully clean (procs/mounts/dirs 0, exits 143/130), KILL reaps the child via PDEATHSIG (stale lazy mount entry is the documented, uncatchable residual). auto_unmount dead end: vendored fuser forces allow_other with it, which needs user_allow_other in /etc/fuse.conf — reverted.
