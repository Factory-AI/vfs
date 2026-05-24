# Session Tail

## user


## assistant
# Read-Only Review: Phase 5 Partial-Origin Overlay

## Actions Taken

- Read the Phase 5 north-star spec, `SPEC.md`, `TESTING.md`, `overlayfs.rs`, `agentfs.rs`, and `schema.rs`.
- Inspected overlay partial-origin schema, read/write/truncate paths, rename/link/unlink interactions, HostFS inode behavior, migration/snapshot-related code, and current tests.
- No files were edited or written.
- Validators/tests were not run because this was explicitly a read-only review.

## Blockers

### 1. Partial-origin files are not reopen/snapshot safe because persisted `base_ino` is a volatile HostFS inode

**Refs:**
- `sdk/rust/src/filesystem/overlayfs.rs:193-199`
- `sdk/rust/src/filesystem/overlayfs.rs:516-530`
- `sdk/rust/src/filesystem/overlayfs.rs:904-912`
- `sdk/rust/src/filesystem/overlayfs.rs:1263-1268`
- `sdk/rust/src/filesystem/hostfs_linux.rs:202-229`
- `sdk/rust/src/filesystem/hostfs_linux.rs:384-389`

`fs_partial_origin` persists `base_ino`, but `HostFS` inode numbers are per-process/per-HostFS virtual cache entries. The existing lookup code already documents that `fs_origin.base_ino` can be stale after remount, but `partial_file_for_delta()` still opens the base file directly by persisted `base_ino`. After remount/snapshot restore, partial-origin reads can fail or read the wrong base handle.

**Suggested fix:** load and use `base_path` for partial origins; resolve it from base root on reopen/open, verify identity/fingerprint, then open that live base inode. Add remount and main-DB snapshot restore tests for partially modified files.

### 2. Rename/hardlink of already-partial delta files can leave stale overlay paths and whiteout the live inode

**Refs:**
- `sdk/rust/src/filesystem/overlayfs.rs:401-407`
- `sdk/rust/src/filesystem/overlayfs.rs:436-470`
- `sdk/rust/src/filesystem/overlayfs.rs:1818-1852`
- `sdk/rust/src/filesystem/overlayfs.rs:1887-1923`

Once a base file is partial-copied up, `reverse_map` maps the delta inode to an overlay inode. `get_or_create_overlay_ino()` returns that inode without updating its stored path. `rename()` does not call `refresh_overlay_mapping()` after moving a delta-backed partial file. Then source whiteout creation can make the stale `info.path` whiteouted, so subsequent open/getattr via the renamed path can report `NotFound`. Hardlinks have the same single-path-per-inode hazard after unlinking the original path.

**Suggested fix:** update overlay mapping after delta renames, and revisit hardlink representation so whiteout checks are path/dentry-aware rather than tied to one inode path. Add partial-origin rename/link/unlink tests, including unlinking the original hardlink path.

## Major

### 1. No base drift detection despite persistent base fallback

**Refs:**
- `sdk/rust/src/filesystem/overlayfs.rs:193-199`
- `sdk/rust/src/filesystem/overlayfs.rs:536-548`
- `sdk/rust/src/filesystem/overlayfs.rs:1219-1228`

The schema stores only `base_ino`, `base_path`, and mutable `base_size`; it does not store mtime/ctime/nsec, device/inode identity, or fingerprint. Reads of unmodified chunks fall through to the current base contents, so external base changes silently alter overlay-visible data.

**Suggested fix:** store base identity and drift metadata/fingerprint when creating the partial origin; verify on reopen/read and either fail loudly or detach to full delta ownership.

### 2. Metadata operations still perform whole-file copy-up under partial-origin mode

**Refs:**
- `sdk/rust/src/filesystem/overlayfs.rs:1498-1559`
- `sdk/rust/src/filesystem/overlayfs.rs:770-782`

`chmod`, `chown`, and `utimens` call `copy_up_and_update_mapping()` for base files, which copies all data into the delta. The Phase 5 spec calls for metadata changes to remain delta-local.

**Suggested fix:** in partial-origin mode, create a metadata/partial-origin delta inode without copying file contents for regular files, then apply metadata to the delta inode.

## Minor

### Test coverage is too narrow for the risk profile

**Refs:**
- `sdk/rust/src/filesystem/overlayfs.rs:2077-2225`
- `scripts/validation/large-edit-benchmark.py:434-454`
- `scripts/validation/large-edit-benchmark.py:503-514`

Current partial-origin tests cover single-byte write, cross-chunk write, truncate-shrink-extend, and default-off behavior. They do not cover remount/snapshot restore, base drift, rename/link/unlink after partial copy-up, stale origin recovery, or hardlink whiteout behavior. The large-edit benchmark also does not set `AGENTFS_OVERLAY_PARTIAL_ORIGIN`, so it measures default whole-file copy-up unless the caller sets the env var externally.

**Suggested fix:** add targeted partial-origin tests for remount/snapshot, rename, unlink, hardlink, base drift, and benchmark execution with the opt-in flag enabled.
