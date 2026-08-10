# Local patches

Upstream: `turso` 0.7.2 from crates.io.

This fork exposes the strict read-only database open required for immutable
Vfs parent artifacts. Upstream's `Builder` always requests writable open
flags, even when callers only need reads.

Functional delta:

- `Builder` stores a `read_only` flag, defaulting to `false`.
- `Builder::read_only(bool)` selects the open mode.
- `Builder::build` passes the flag through `TursoDatabaseConfig`.
- The sync builder explicitly keeps its existing writable behavior by setting
  `read_only: false`.

This fork also routes `Connection::execute` and `Connection::query` through
the per-connection statement cache (`prepare_maybe_cached`). Upstream prepares
every one-shot statement fresh, and statement translation got measurably more
expensive across 0.5.x → 0.7.x; caching removes that cost from every hot
caller, including `Transaction`'s `BEGIN`/`COMMIT`/`ROLLBACK` which deref to
these methods. PRAGMA statements are exempt from the cache because pragma
assignments apply their side effect at translate time and the setters that do
not bump the prepare-context generation (e.g. `fullfsync`) would skip that
effect on a cache hit.

No other behavior is changed.
