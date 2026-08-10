# Vendored turso_core patches

Vendored from crates.io `turso_core 0.7.2` (the registry source, verbatim
except for the change below). The workspace points every `turso_core`
dependency here via `[patch.crates-io]`, so the companion vendored `turso`
and `turso_sdk_kit` trees build against this copy.

## Statement drop must not invalidate the prepared-statement cache

Functional delta (one function, `connection.rs`):

- `rollback_attached_wal_txns` bumps the prepare-context generation only
  when it actually removed an entry from `database_schemas`, mirroring the
  `cleared_any_schema` gate its MVCC sibling already has.

Why: every `Statement` drop runs `reset_best_effort` → `Program::abort` →
`rollback_current_txn_state` → `rollback_attached_wal_txns`. Upstream bumps
the generation unconditionally whenever any non-main pager exists — and the
TEMP database's pager always exists once `BEGIN IMMEDIATE` has run on the
connection, because SQLite-parity opcode emission initializes TEMP lazily in
`op_transaction`. The bump invalidates every cached prepared statement on
the connection, so each statement re-prepared on next use: ~89,000
re-prepares in one git-clone workload, all parser/translate/allocator cost
on the hot path.

The gate is behavior-preserving for real ATTACH users: detaching or rolling
back an attached database whose schema was cached still discards the cache
entry and still bumps. TEMP never stores its schema in `database_schemas`
(its single source of truth is `temp_db.db.schema`), so the no-op case no
longer pays the invalidation.

No other behavior is changed.
