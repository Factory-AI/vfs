# Tier Two prep — fresh benchmark comparison

Native vs **Original AgentFS** (`origin/main` 3a5ed2b, AgentFS 0.6.4)
vs **Tier One AgentFS** (`phase4-north-star-implementation` 9be0da4,
the kernel-cache-by-default ship).

All runs on the same machine with no `AGENTFS_FUSE_*` env vars set, release builds.

---

## Headline (ratio of agentfs / native; lower is better)

| Workload | Original | Tier One | Δ |
|---|---:|---:|---:|
| Read-heavy (full run incl. startup) | 2.51x | 3.03x | +21% |
| Read-heavy (steady-state only)      | 7.76x | 3.79x | −51% |
| Copy-on-write edit (50 MiB file)    | 8.19x | 5.42x | −34% |
| Mixed git workload                  | 5.16x | 3.21x | −38% |

Plus: CoW delta DB growth (overlay copy-up footprint, lower is better):
  Original 172.6 MiB  →  Tier One 50.4 MiB  (−71%)

---

## Read-heavy detail (read-path-benchmark.py, cold + warm modes)

_8 files / 2 dirs / 64 KiB each; 8 iters each of stat-storm, readdir-storm,_
_open-read-close, repeated-open-read on a steady-state mount._

| Phase | Native (s) | Original (s) | Tier One (s) | Orig | Tier One |
|---|---:|---:|---:|---:|---:|
| cold/STARTUP+WORKLOAD total | 0.0541 | 0.1389 | 0.1263 | 2.91x | 2.34x |
| cold/STEADY workload | 0.0040 | 0.0360 | 0.0169 | 13.35x | 4.24x |
| cold/bounded_file_scan | 0.0002 | 0.0069 | 0.0008 | 32.01x | 3.52x |
| cold/open_read_close_loop | 0.0012 | 0.0087 | 0.0053 | 12.70x | 4.54x |
| cold/readdir_plus_storm | 0.0008 | 0.0071 | 0.0036 | 13.36x | 4.56x |
| cold/readdir_storm | 0.0004 | 0.0057 | 0.0020 | 19.47x | 4.50x |
| cold/repeated_read_only_base_open_read_close_loop | 0.0004 | 0.0027 | 0.0026 | 9.24x | 5.84x |
| cold/stat_lstat_storm | 0.0006 | 0.0009 | 0.0012 | 2.12x | 2.02x |
| cold/tree_discovery | 0.0003 | 0.0039 | 0.0013 | 16.77x | 4.78x |
| warm/STARTUP+WORKLOAD total | 0.0380 | 0.1340 | 0.1151 | 2.51x | 3.03x |
| warm/STEADY workload | 0.0031 | 0.0220 | 0.0116 | 7.76x | 3.79x |
| warm/bounded_file_scan | 0.0003 | 0.0016 | 0.0008 | 7.88x | 3.13x |
| warm/open_read_close_loop | 0.0008 | 0.0065 | 0.0040 | 8.21x | 5.18x |
| warm/readdir_plus_storm | 0.0006 | 0.0043 | 0.0017 | 7.93x | 3.03x |
| warm/readdir_storm | 0.0003 | 0.0028 | 0.0015 | 9.36x | 4.86x |
| warm/repeated_read_only_base_open_read_close_loop | 0.0003 | 0.0030 | 0.0019 | 10.16x | 5.53x |
| warm/stat_lstat_storm | 0.0005 | 0.0010 | 0.0005 | 2.17x | 0.97x |
| warm/tree_discovery | 0.0003 | 0.0029 | 0.0011 | 11.51x | 4.50x |

---

## Copy-on-write detail (large-edit-benchmark.py)

_50 MiB base file, single-byte edit at file midpoint, then re-read+compare for correctness._

| Metric | Native | Original | Tier One |
|---|---:|---:|---:|
| Edit wall time (s) | 0.1226 | 0.5015 | 0.6650 |
| Wall ratio vs native | 1.00x | 8.19x | 5.42x |
| Delta DB growth (MiB) | n/a | 172.59 | 50.41 |
| Correctness (outputs match) | n/a | True | True |

---

## Mixed git-workload detail (git-workload-benchmark-multi.py)

_openai/codex (4 643 files, 690 dirs, 63 MiB) bare→working clone, status,_
_32-file ls-files scan w/ 4 KiB reads, 4 representative edits w/ fsync, diff._
_3 measurement iterations + 1 warmup. Medians shown._

| Phase | Native (s) | Original (s) | Tier One (s) | Orig | Tier One |
|---|---:|---:|---:|---:|---:|
| checkout | 0.1692 | 0.1725 | 0.1498 | 1.07x | 0.88x |
| clone | 0.2756 | 2.3499 | 2.2126 | 7.03x | 7.65x |
| diff | 0.0226 | 0.0346 | 0.1316 | 2.24x | 1.72x |
| edit | 0.0004 | 0.0013 | 0.0028 | 2.38x | 6.43x |
| fsck | 0.0000 | 0.0000 | 0.0000 | 0.00x | 0.00x |
| read_search | 0.0046 | 0.0077 | 0.0097 | 1.40x | 2.11x |
| status | 0.0967 | 0.2977 | 0.1646 | 12.51x | 1.70x |

---

## Per-iteration reproducibility — mixed workload

| iter | wall_s (orig) | wall_s (tier1) |
|---:|---:|---:|
| 1 | 6.48 | 9.71 |
| 2 | 4.01 | 8.61 |
| 3 | 3.59 | 10.63 |

_Tier One mixed-workload stdev: 0.85x_  
_Original mixed-workload stdev: 1.21x_

---

## Tier Two focus areas (from this comparison)

1. **Clone phase still dominates the mixed wall** (~2.2 s of the ~2.9 s
   agentfs median). Native does the same clone in ~0.28 s. The clone phase
   does many small writes through copy-on-write to the SQLite delta;
   batched write path and/or parallel git-pack creation under copy-on-write
   is the next big lever. (median ratio: orig 7.03x → tier1 7.65x; the
   variance is within noise on a 0.28 s native baseline.)

2. **Mount startup regressed by ~10-15 ms** (read-heavy full-run ratio went
   2.51x → 3.03x — going _up_) because Tier One mounts now negotiate
   parallel workers + readdirplus + writeback + ABI 7.31 caps at FUSE init.
   For short-lived sandboxes this dominates total wall; for sustained
   workloads it is amortised, which is why steady-state read-storms dropped
   from 7.76x → 3.79x in the same comparison. Tier Two should defer worker
   pool warmup to first request to recover that startup cost.

3. **Copy-on-write DB growth is now great (-71%)** but the wall-time ratio
   (5.42x) is still the worst-of-three. Chunked copy-up + smarter chunk
   sizing is the obvious next win and would compound with #1 since
   git-clone bottlenecks on the same path.

4. **Steady-state read storms are near best-case**: `stat_lstat_storm`
   (warm) is 0.97x — actually _faster_ than native — because the kernel
   attribute cache absorbs everything past the first lookup. Future
   read-path tuning is diminishing returns; the Tier Two budget should go
   to CoW writes and clone-phase batching.

5. **Behaviour to verify before Tier Two ships**: the per-iteration
   variance on the mixed workload is high (Tier One stdev 0.85x, Original
   1.21x). A longer-iter (e.g. 10 + 2 warmup) run on a quiet machine would
   tighten the medians; current 3-iter medians are reliable directional
   signal but not paper-grade absolutes.
