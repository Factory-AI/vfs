# Session Tail

## user


## assistant
# Summary

Implemented an honest unsupported Phase 6.5 passthrough prototype path in `/home/ain3sh/factory/vfs-phase65-worktrees/passthrough`: eligibility is now explicit, profiled, and falls back safely to HostFS without handing any fd to the kernel.

# Support Status

`passthrough_supported=false`.

Finding: vendored FUSE has partial `abi-7-40` shape/reply stubs, but the active build does not expose a complete backing-fd registration path, so real kernel passthrough is **not feasible yet**. Eligible opens now record attempted/fallback counters; succeeded remains `0`.

# Files Changed

- `/home/ain3sh/factory/vfs-phase65-worktrees/passthrough/cli/src/fuse.rs`
- `/home/ain3sh/factory/vfs-phase65-worktrees/passthrough/sdk/rust/src/filesystem/mod.rs`
- `/home/ain3sh/factory/vfs-phase65-worktrees/passthrough/sdk/rust/src/filesystem/overlayfs.rs`
- `/home/ain3sh/factory/vfs-phase65-worktrees/passthrough/sdk/rust/src/profiling.rs`

Smoke artifacts written:

- `/tmp/agentfs-profile-smoke.H5Wy4X/stdout.txt`
- `/tmp/agentfs-profile-smoke.H5Wy4X/stderr.txt`
- `/tmp/agentfs-profile-smoke.H5Wy4X/.agentfs/smoke.db`
- `/tmp/agentfs-profile-smoke.H5Wy4X/.agentfs/smoke.db-wal`

# Tests / Validation Run

- `cargo fmt --check` for SDK and CLI
- `CARGO_BUILD_JOBS=1 cargo check --manifest-path cli/Cargo.toml --no-default-features`
- `CARGO_BUILD_JOBS=1 cargo test --manifest-path sdk/rust/Cargo.toml test_overlay_read_only_passthrough_eligibility_rejects_unsafe_inputs -- --nocapture`
- `CARGO_BUILD_JOBS=1 cargo test --manifest-path sdk/rust/Cargo.toml profiling::tests -- --nocapture`
- `CARGO_BUILD_JOBS=1 cargo test --manifest-path cli/Cargo.toml --no-default-features fuse_ -- --nocapture`
- `AGENTFS_PROFILE=1 cargo run --manifest-path cli/Cargo.toml --no-default-features -- init smoke`

Profile smoke confirmed:

```json
"passthrough_supported": false,
"fallback_read_path": "hostfs"
```

# Next Steps / Blockers

Real passthrough requires completing vendored FUSE backing-fd registration support before enabling any kernel fd handoff. No commit or push was performed.
