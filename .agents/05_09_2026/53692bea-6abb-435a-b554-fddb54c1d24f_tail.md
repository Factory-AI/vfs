# Session Tail

## user


## assistant
# Read-Only Review Results

## Concrete Actions Taken

- Read `README.md`, `SPEC.md`, the Phase 4 north-star spec, and all requested changed files.
- Used targeted codebase search for schema init/detection, migration, chunk/inline paths, and overlay table interactions.
- Reviewed current branch diff/stat context and gathered line-numbered references.
- Wrote **no files**.

## Findings

### Blocker — `migrate-v0-5` does not preserve/activate Rust overlay configuration

**Files/lines:**
- `cli/src/cmd/migrate.rs:323-330`
- `sdk/rust/src/filesystem/overlayfs.rs:104-115`
- `cli/src/cmd/mount.rs:151-177`, `230-255` from search context

**Why it matters:**  
Rust overlay databases store `fs_overlay_config.base_path`; mount uses that table to decide whether to re-open the DB as an overlay. The copy migration creates/copies `fs_whiteout` and `fs_origin`, but never creates or copies `fs_overlay_config`. A migrated overlay DB will mount as plain AgentFS, so base-layer visibility/whiteout/origin semantics are lost.

**Suggested fix:**  
Create and copy `fs_overlay_config` during v0.5 migration, then include it in verification.

---

### Blocker — Migrated `fs_whiteout` schema is incompatible with current Rust overlay code and legacy Rust overlay DBs

**Files/lines:**
- `cli/src/cmd/migrate.rs:327`, `429-433`, `688-745`, `786-791`
- `sdk/rust/src/filesystem/overlayfs.rs:96-100`, `211-214`

**Why it matters:**  
Migration target creates `fs_whiteout(path, parent_path NOT NULL, created_at)`, but current Rust overlay creates and writes `fs_whiteout(path, created_at)` only. Consequences:
- Migrating a Rust-created overlay DB with `fs_whiteout(path, created_at)` fails when copying rows into the target because `parent_path` is omitted.
- With `--verify`, even an empty legacy Rust `fs_whiteout` table fails because verification queries `parent_path` from the source.
- After migration, Rust overlay `create_whiteout()` still inserts only `path, created_at`, so new whiteouts fail against the migrated v0.5 schema.

**Suggested fix:**  
Unify Rust overlay schema/write path with the v0.5/spec `parent_path` column, and make migration synthesize `parent_path` from `path` for legacy source tables that lack it. Verification should compare appropriately normalized rows.

---

### Major — Legacy `agentfs migrate` reports/records current v0.5 without creating v0.5 schema

**Files/lines:**
- `cli/src/cmd/migrate.rs:53-75`, `123-138`
- CLI help says “Migrate database schema to the current version” at `cli/src/opts.rs:326`

**Why it matters:**  
For v0.4, `agentfs migrate` prints “already at latest” even though current schema is `0.5`. For v0.0/v0.2, it only applies old in-place migrations to v0.4, then writes `schema_version = 0.5` without adding `data_inline`, `storage_kind`, or `inline_threshold`. That leaves a misleading DB that still detects as v0.4 and cannot be opened by the SDK.

**Suggested fix:**  
Make the legacy migrate command explicit: migrate only to v0.4 and do not write `AGENTFS_SCHEMA_VERSION`, or route/error with instructions to use copy-based `migrate-v0-5 <source> <target>`. Do not claim v0.4 is latest.

---

### Major — Copy migration materializes whole files, making sparse/large-file migration unsafe

**Files/lines:**
- `cli/src/cmd/migrate.rs:580-590`, `636-685`, `849-905`

**Why it matters:**  
`read_source_file_bytes()` allocates `vec![0; size]` for every regular file. Sparse files with huge logical sizes can OOM the migration, and chunked targets are built from the fully materialized byte vector, expanding holes into stored zero data. Verification repeats the same full-file materialization for source and target.

**Suggested fix:**  
Use bounded streaming rechunking. Only read dense small files into memory for inline eligibility; for chunked files, stream source chunks into 64KiB target chunks while preserving holes/omitting zero-only gaps. Verification should compare streamed ranges or hashes without allocating full logical file size.

---

### Major — Source unchanged verification only hashes the main DB file and does not provide a stable source snapshot

**Files/lines:**
- `cli/src/cmd/migrate.rs:277-289`, `350-353`, `1121-1132`

**Why it matters:**  
The migration opens the source read/write through `Builder::new_local`, does not hold a single source read transaction/snapshot for the copy, and hashes only the main `.db` file, ignoring `-wal`/`-shm`. Concurrent source writes or WAL-sidecar changes can escape the “source hash unchanged” check, and the target can be copied from a moving source.

**Suggested fix:**  
Open source read-only/immutable where supported, or hold a consistent read transaction for the entire copy. Checkpoint or include the DB family/WAL state in source-change detection, and fail before committing/keeping target if the source changes.

## Blocker Status

Blockers found: **yes**.

## Files Written

None.

## Uncertainties / Follow-Ups

- I did not run tests or validators because this was explicitly a read-only review task.
