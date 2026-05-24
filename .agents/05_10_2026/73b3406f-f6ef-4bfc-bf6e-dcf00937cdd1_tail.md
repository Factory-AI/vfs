# Session Tail

## user


## assistant
# Read-Only Review Result

## Actions Taken

- Read all requested spec/docs/source/validation files under `/home/ain3sh/factory/vfs`.
- Inspected partial-origin overlay paths, POSIX profiles, macOS NFS validation, NFS handle/write-authority code, and large-edit benchmark flag handling.
- Wrote no files.

## Blockers

**No blockers found.**

## Major Findings

### 1. Partial-origin `O_TRUNC` open does not truncate

- **Refs:** `sdk/rust/src/filesystem/overlayfs.rs:41-42`, `1056-1077`, `1792-1815`
- `is_write_open()` treats `O_TRUNC` as a write-open trigger, but the partial-origin path returns an `OverlayPartialFile` directly via `partial_file_for_delta()` without applying the truncate semantics.
- This can leave stale base-backed contents visible after an `open(..., O_TRUNC)` path.
- **Suggested fix:** after partial copy-up, if `flags & libc::O_TRUNC != 0`, call `truncate(0)` on the returned partial file before returning it; add SDK coverage for `O_TRUNC` on both fresh base and existing partial-origin delta files.

### 2. NFS `SETATTR` guard mismatch still applies mutation

- **Refs:** `cli/src/nfsserve/nfs_handlers.rs:1850-1862`
- On `sattrguard3::obj_ctime` mismatch, the handler serializes `NFS3ERR_NOT_SYNC` but does not return, so it continues into `context.vfs.setattr(...)` and may also emit a second response.
- **Suggested fix:** return immediately after serializing `NFS3ERR_NOT_SYNC`, and add an NFS handler test proving guarded `SETATTR` does not mutate.

## Minor Findings

### 1. macOS mount detection may falsely fail under `/tmp` canonicalization

- **Refs:** `scripts/validation/macos-nfs-git-validation.sh:56-58`, `150-151`, `176-188`
- The script creates mount dirs under `/tmp` but detects mounts by string-matching `mount` output against that exact path. On macOS, `/tmp` commonly resolves to `/private/tmp`, so a successful mount can be missed.
- **Suggested fix:** canonicalize `WORK_DIR`/`MOUNT_DIR` with `pwd -P` after `mktemp`, or make `is_mounted` compare canonical paths.

### 2. NFS write-token bookkeeping is bounded but not ordered/strongly random

- **Refs:** `cli/src/nfs.rs:40-42`, `78-86`, `192-198`
- Write tokens use only 64 bits from a UUID, `fh_generation` is timestamp-derived, and token eviction removes an arbitrary `HashMap` key despite the `oldest` variable name.
- **Suggested fix:** use a 128-bit CSPRNG token, randomize `fh_generation`, and store insertion order/LRU for deterministic bounded eviction.

## Nit Findings

### POSIX known-gap taxonomy comment is broader than the TSV taxonomy

- **Refs:** `scripts/validation/posix/pjdfstest/phase5-ci.txt:5-7`, `scripts/validation/posix/pjdfstest/known-gaps.tsv:1`
- `phase5-ci.txt` says excluded categories include `chflags`, read-only mount setup, ENOSPC setup, and OS-specific quick-exit checks, but the known-gap TSV taxonomy does not include explicit rows for those categories.
- **Suggested fix:** add explicit taxonomy rows for those excluded categories or narrow the profile comment.

## No Finding

- Large-edit partial-origin flag semantics look consistent: CLI/env defaulting, `--partial-origin` / `--no-partial-origin`, env application/removal, and JSON reporting are wired coherently (`large-edit-benchmark.py:163-177`, `461-470`, `593-597`).
