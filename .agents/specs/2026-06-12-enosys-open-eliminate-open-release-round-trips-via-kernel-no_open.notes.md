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

## 2026-07-02T17:20-07:00 — uring+noopen compound: read-heavy wins, uring stays opt-in
**Type**: decision
**Context**: With fuse.enable_uring re-enabled (reset by the kernel upgrade), compound A/B on the noopen default: micro open/read/close 25.1 -> 19.3us/cycle (paired median 0.848, 5/6 pairs), git workload read_search -32.6%, diff -13.5%, status -12.9%, fsck -2.3% — but clone +13.8%, checkout +10.9% (write-heavy phases; per-CPU queue threads compete with SQLite workers), edit spike again noise-shaped, total +9.8%.
**Resolution**: Same shape as the WS6 verdict: uring compounds well on RT-bound reads and costs on write-heavy phases. AGENTFS_FUSE_URING=1 stays opt-in; recommended for read-dominated workloads only. No default change.

## 2026-07-02T17:25-07:00 — SDK followup closed: POSIX unlink-while-open via deferred inode reaping
**Type**: decision
**Context**: The gate-documented gap (immediate inode reap made any I/O — even the close-time writeback mtime SETATTR — fail on an unlinked-open file, both modes) is now fixed at the SDK layer. `OpenInodes` registry: every user-visible AgentFSFile carries an RAII guard; unlink/rename-replace (all four deletion sites, public + trait) defer row deletion when handles are live, leaving `nlink = 0` as the crash-safe orphan marker. Last-handle drop queues the ino; `process_deferred_reaps` (hooked at trait unlink/rmdir/rename and finalize, guarded by `nlink = 0` against rowid reuse) deletes rows; a mount-time sweep collects crash-stranded orphans. Integrity invariant `namespace.non_root_inode_has_dentry` amended: dentry-less is legal iff nlink = 0.
**Resolution**: noopen-coherence scenario 5 restored to full POSIX assertions (read-back, write-through, fsync, st_nlink==0, clean close) — 6/6 PASS in both modes; SDK 168 tests (2 new: deferred reap, mount sweep); all light gates green. Documented residuals: (a) under noopen, an ino_files LRU-cap eviction of a clean entry drops the SDK handle early, so a >65k-simultaneous-inode workload could still lose an orphan's rows before the kernel fd closes (kernel-side open counts are exactly what no_open discards); (b) cross-mount: a second mount's sweep cannot see this process's open handles — equivalent-or-better than the pre-fix instant reap in both cases. Reap laziness (space held until next namespace mutation/finalize) is POSIX-conformant.
