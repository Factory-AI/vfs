# Local patches

Upstream: `turso` 0.5.3 from crates.io.

This fork exposes the strict read-only database open required for immutable
Vfs parent artifacts. Upstream's `Builder` always requests writable open
flags, even when callers only need reads.

Functional delta:

- `Builder` stores a `read_only` flag, defaulting to `false`.
- `Builder::read_only(bool)` selects the open mode.
- `Builder::build` passes the flag through `TursoDatabaseConfig`.
- The sync builder explicitly keeps its existing writable behavior by setting
  `read_only: false`.

No other behavior is changed.
