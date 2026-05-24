# Tier Four — fresh benchmark comparison

Native vs **Tier Three AgentFS** (`phase4-north-star-implementation` 17292de,
SDK batcher default-on + 50% worker default + 16 KiB inline) vs **Tier Four
AgentFS** (HEAD, consistent-without-drain read overlay + FUSE flush_pending
no longer drains).

5-iter and 9-iter aggregates on the same machine, codex fixture,
`AGENTFS_FUSE_WRITEBACK=1` (default).

---

## Headline (9-iter median, codex fixture)

| Workload                                              | Tier Two | Tier Three | Tier Four |
| ----------------------------------------------------- | -------: | ---------: | --------: |
| Mixed git workload — ratio                            |    2.97x |      2.73x |     3.24x |
| Mixed git workload — agentfs absolute (s)             |    2.51  |      2.28  |     2.47  |
| Mixed git workload — native absolute (s)              |    0.85  |      0.82  |     0.72  |
| Mixed ratio stdev                                     |    1.45x |      1.67x |     1.72x |

**Tier 4 did not deliver the spec's ~2.5x ratio target.** Mixed median regressed
slightly vs Tier 3, well within the high noise floor (stdev ~1.7x, per-iter
range 1.61x to 4.71x on the 9-iter run). Native time shrank by ~13% which
amplifies the ratio.

---

## Mixed workload per-phase (9-iter medians, 2 warmups)

| Phase       | Native (s) | Tier 3 (s) | Tier 4 (s) | Δ agentfs |
| ----------- | ---------: | ---------: | ---------: | --------: |
| checkout    |     0.139  |     0.195  |     **0.117** |   **−40%**  |
| clone       |     0.247  |     1.80   |     1.79   |    flat    |
| diff        |     0.011  |     0.117  |     0.175  |    +50%   |
| edit        |     0.000  |     0.003  |     0.004  |    +60%   |
| fsck        |     0.141  |     —      |     0.157  |    —      |
| read_search |     0.005  |     0.009  |     0.014  |    +56%   |
| status      |     0.174  |     0.255  |     0.270  |     +6%   |

Checkout (read-heavy, many opens) improved 40% — the overlay path works as
designed. Diff and read_search regressed by ~50%, traceable to the two extra
`batcher.state.lock().await` acquires per `pread` (peek_pending_max_end +
peek_pending). Absolute regression is small (≤80 ms) and could be eliminated
with an inode-has-pending fast-path flag in Tier 5.

---

## What shipped vs what was attempted

| Axis | Status | Effect |
| --- | --- | --- |
| Tier 4 — consistent-without-drain SDK overlay | **shipped** | architectural foundation; reads no longer force SQLite commit |
| FUSE `flush_pending_inode` drain removal | **shipped** | reads now go directly to overlay; durability via fsync/destroy/timer |
| Lookup conn-pool deadlock fix | **shipped** | `merge_pending_size` helper; lookup no longer drains while holding conn |
| `discard_pending` at unlink/rename/remove | **shipped** | no orphan `fs_data` rows when batched drain runs |
| `attr_cache.remove` on enqueue | **shipped** | cache invalidation on write, not just commit |
| Tier 5 (defer release/forget drain + pack-stream) | **deferred** | next tier; will exploit Tier 4's overlay |
| Tier 6 (shadow tree + FUSE_PASSTHROUGH) | **deferred** | needs Tier 5 go/no-go review first |

---

## Latent bugs surfaced (and fixed)

Tier 4 exposed three pre-existing bugs that the synchronous drain-on-every-op
pattern was hiding:

1. **Single-conn pool deadlock in `lookup`** — held the only conn permit while
   calling `drain_inode_writes` → `drain_pending_batched` (which also needs
   conn). Pre-Tier 4 batcher was always empty so the deadlock was unreachable.
   Fix: `merge_pending_size` helper replaces drain with cheap peek.

2. **Orphan `fs_data` rows on unlink/rename/remove** — batched-drain commits
   ALL pending inodes in one transaction; if an inode was deleted between
   enqueue and drain, the commit fails with `Fs(NotFound)` and aborts the
   whole batch. Fix: `discard_pending` hooked into every inode-delete site.

3. **`cat` after `write_filesystem` saw stale data** — CLI commands open a
   fresh AgentFS per invocation. Without an explicit `drain_all` on writer
   exit, the reader's separate AgentFS instance can't see the bytes (they're
   in the writer's batcher, not SQLite). Fix: `write_filesystem` now calls
   `drain_all` before returning.

---

## Validation

- 157 SDK lib tests pass (148 pre-existing + 9 new Tier 4 overlay tests)
- 106 CLI tests pass after the FUSE refactor
- clippy clean on both sdk and cli
- cargo fmt applied
- Phase 8 smoke: all 7 gates pass

---

## Recommendation: GO on Tier 5

Despite Tier 4's mixed-workload median regressing slightly, the underlying
overlay foundation is correct and tested. Tier 5 (defer release/forget drain +
pack-aware streaming writer) is what actually unlocks the perf win and now has
a safe substrate to build on. The Tier 4-introduced read latency on `diff` /
`read_search` (~50 ms absolute) is small enough that a fast-path inode-has-
pending flag in Tier 5 should reclaim it cheaply.

If Tier 5 doesn't drive mixed median below 2.0x with tight variance, the
Tier 5 → Tier 6 gate in the spec fires and we re-evaluate before the
shadow-tree pivot.
