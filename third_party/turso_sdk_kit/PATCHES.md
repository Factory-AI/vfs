# Local patches

Upstream: `turso_sdk_kit` 0.7.2 from crates.io.

This fork threads an explicit read-only open mode to `turso_core` so immutable
Vfs parent artifacts can be read without creating or modifying any SQLite
file-family member.

Functional delta:

- `TursoDatabaseConfig` has a `read_only` boolean.
- The open state machine's Init phase selects `OpenFlags::ReadOnly` instead of
  the default (`Create`) when requested; the stored `state.open_flags` carries
  it into the async core open, so both the VFS file open and
  `Database::open_with_flags_async` see the same mode. `ReadOnly` also skips
  the OS-level file lock in `turso_core`'s unix backend, which is what lets
  many branch mounts read one parent artifact concurrently.
- The C API conversion and the crate's own tests explicitly default to
  writable mode (`read_only: false`).

This fork also bounds the per-connection prepared-statement cache
(`STATEMENT_CACHE_CAP`, 512 entries): the companion `turso` patch routes all
one-shot `execute`/`query` statements through `prepare_cached`, so a workload
generating unbounded statement shapes (variable IN lists, multi-row VALUES
inserts) would otherwise grow the cache without limit. At the cap the whole
map is dropped; hot statements re-enter on next use.

No other behavior is changed.
