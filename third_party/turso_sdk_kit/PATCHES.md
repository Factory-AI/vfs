# Local patches

Upstream: `turso_sdk_kit` 0.5.3 from crates.io.

This fork threads an explicit read-only open mode to `turso_core` so immutable
Vfs parent artifacts can be read without creating or modifying any SQLite
file-family member.

Functional delta:

- `TursoDatabaseConfig` has a `read_only` boolean.
- The initial VFS file open uses `OpenFlags::ReadOnly` when requested.
- The asynchronous core database open uses `OpenFlags::ReadOnly` when
  requested.
- The unchanged C API and existing tests explicitly default to writable mode.

No other behavior is changed.
