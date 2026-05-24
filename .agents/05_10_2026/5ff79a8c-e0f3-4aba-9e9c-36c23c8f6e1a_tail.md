# Session Tail

## user


## assistant
# Prototype Complete

## Actions & Outcomes

- Researched vendored FUSE support:
  - `FOPEN_KEEP_CACHE` and `FOPEN_DIRECT_IO` are defined in `cli/src/fuser/ll/fuse_abi.rs`.
  - `ReplyOpen::opened(fh, flags)` already serializes flags into `fuse_open_out.open_flags`.
  - `FUSE_DO_READDIRPLUS`, `FUSE_READDIRPLUS_AUTO`, and kernel capability gating are present, but I did not enable readdirplus auto in this patch.

## Patch Made

Implemented a safe `FOPEN_KEEP_CACHE` prototype:

- Added `FileSystem::keep_cache_for_read_open(...) -> Result<bool>` with conservative default `false`.
- Implemented OverlayFS gating:
  - `true` only for regular files currently backed by the base layer.
  - `false` for writable opens, delta-backed files, whiteouts, and after copy-up/delta mutation.
- FUSE `open` now replies with `FOPEN_KEEP_CACHE` only when that gate returns `true`.
- Added deferred `inval_inode` support and invalidate-on-truncate to reduce stale page-cache risk for truncate/copy-up paths that do not go through a writable FUSE open reply.

## Files Changed

- `/home/ain3sh/factory/vfs-phase6-worktrees/fuse-flags/cli/src/fuse.rs`
- `/home/ain3sh/factory/vfs-phase6-worktrees/fuse-flags/cli/src/fuser/deferred_notify.rs`
- `/home/ain3sh/factory/vfs-phase6-worktrees/fuse-flags/cli/src/fuser/session.rs`
- `/home/ain3sh/factory/vfs-phase6-worktrees/fuse-flags/sdk/rust/src/filesystem/mod.rs`
- `/home/ain3sh/factory/vfs-phase6-worktrees/fuse-flags/sdk/rust/src/filesystem/overlayfs.rs`

## Validation

Passed:

```bash
cargo fmt --manifest-path cli/Cargo.toml -- --check
cargo fmt --manifest-path sdk/rust/Cargo.toml -- --check
CARGO_BUILD_JOBS=1 cargo check --manifest-path cli/Cargo.toml --no-default-features -j 1
CARGO_BUILD_JOBS=1 cargo test --manifest-path cli/Cargo.toml --no-default-features -j 1
CARGO_BUILD_JOBS=1 cargo test --manifest-path sdk/rust/Cargo.toml -j 1
```

Default CLI check with sandbox enabled is blocked by existing stable-toolchain failure in `reverie-ptrace` using `#![feature(internal_output_capture)]`.

## Impact / Risk

Estimated impact: improved repeated read-only base-file opens by preserving kernel page cache instead of invalidating on every open. Risk is low-to-moderate: the flag is only set for read-only base regular files, never for writable base passthrough, and delta/copy-up paths disable the flag.
