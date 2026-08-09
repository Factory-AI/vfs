# Agent Filesystem Specification

**Version:** 0.8

## Introduction

The Agent Filesystem Specification defines a SQLite schema for representing agent filesystem state. The current v0.8 format stores replayable row-delta history and immutable root snapshots over the v0.7 content-addressed layout, with `user_version`-keyed migration from older databases (in place by default, or copy-based re-chunking with `--copy`). The specification consists of five main components:

1. **Tool Call Audit Trail**: Captures tool invocations, parameters, and results for debugging, auditing, and performance analysis
2. **Virtual Filesystem**: Stores agent artifacts (files, documents, outputs) using a Unix-like inode design with support for hard links, proper metadata, and efficient file operations
3. **Session Handoff Metadata**: Stores the monotonic pack generation and future seed provenance inside the transferable database
4. **Replayable History**: Records committed table-row post-images and immutable root snapshots without duplicating chunk bytes
5. **Key-Value Store**: Provides simple get/set operations for agent context, preferences, and structured state that doesn't fit into the filesystem model

All timestamps in this specification use Unix epoch format (seconds since 1970-01-01 00:00:00 UTC) with optional nanosecond precision via separate `_nsec` columns.

## Runtime Architecture and Safety Invariants

The persistent Vfs authority is the SQLite database described by this
specification. Runtime mounts, caches, file handles, FUSE lookup references, and
overlay inode maps are acceleration structures only; they MUST be reconstructible
from the database plus the configured read-only base path and MUST NOT become
the only source of virtual filesystem state.

Vfs sandboxing is built around two invariants:

1. A portable Vfs database contains all writable virtual filesystem state.
   Clean shutdown SHOULD checkpoint transient SQLite sidecars so backups and
   materialized copies can be represented as a single main database file.
2. Copy-on-write sandbox writes MUST NOT modify the real filesystem. Overlay
   backends MAY read from an explicitly scoped base directory, but file creates,
   writes, truncates, chmod/chown/utimens, links, renames, and deletes are
   represented in the Vfs delta database and overlay metadata.

Implementations MAY use kernel caches, positive/negative lookup caches,
attribute caches, read-dir caches, and parallel FUSE dispatch, provided they
preserve POSIX lookup reference accounting. In particular, any cached positive
lookup reply that creates a kernel lookup reference MUST either reach the backing
filesystem lookup path or explicitly retain the backing inode reference before
replying; later `FORGET` requests must release the same reference count.
Namespace mutations MUST invalidate affected cached dentries and attributes
before the mutation is considered visible to the caller.

The subsections below describe the acceleration structures the reference
implementation actually ships and the invariants each one must preserve.
Every one of them has a declared kill switch or policy knob in the generated
knob ledger (`docs/KNOBS.md`).

### Write Batching and Durability

Writes MAY be acknowledged from an in-memory pending map before their bytes
reach SQLite. The reference implementation batches FUSE writeback-cache
writes and drains them on a short timer window, a per-inode pending-byte
trigger, a global pending-byte cap, and bounded per-transaction inode/byte
budgets (`VFS_BATCH_*` knobs). Buffered acknowledgement is only permitted
for volatile-durability writes; any operation that promises durability —
`fsync`, an NFSv3 `WRITE` acknowledged as `FILE_SYNC`, or unmount/shutdown
finalization — MUST NOT return until the affected pending bytes are committed
to the database (a per-inode or filesystem-wide commit barrier).

Pending state is an acceleration structure, never a second authority:
metadata reads (`stat`, directory attributes) MUST merge pending sizes and
times so applications observe their own buffered writes, and deletions MUST
discard the dead inode's pending bytes. On unclean termination, volatile
(never-fsynced) bytes MAY be lost, but the database MUST remain consistent:
a crash may lose the tail of un-synced data, never corrupt committed state.

### FUSE no-open / no-flush Lifecycle

On Linux the FUSE adapter runs with zero-message opens and zero-message
flush by default: it answers `OPEN`/`RELEASE` (and close-time `FLUSH`) with
`ENOSYS`, after which the kernel stops sending them and issues data requests
with `fh = 0`. Implementations using this mode MUST NOT key any state to
open/release pairs. Per-inode resources (backing file handles for base reads,
caches) are keyed by inode number, held in a bounded LRU
(`VFS_FUSE_INO_FILES_CAP`), and reclaimed by kernel `FORGET` traffic and
LRU eviction rather than by `RELEASE`.

Consequences that MUST hold:

1. Unlink-while-open works through lookup-reference accounting: an unlinked
   inode's rows are reaped only after the kernel drops its last lookup
   reference (deferred reap), not at `RELEASE` time.
2. Close does not imply commit. With no-flush enabled the kernel has already
   written back dirty pages before close; durability is promised only by
   `fsync` (see the durability contract above).
3. Both behaviors retain kill switches (`VFS_FUSE_NOOPEN`,
   `VFS_FUSE_NOFLUSH`) and dedicated coherence gates
   (`scripts/validation/noopen-coherence.py`,
   `scripts/validation/flush-coherence.py`) that validate the default and
   disabled legs.

### FUSE-over-io_uring Transport

On kernels that expose `/sys/module/fuse/parameters/enable_uring = Y`, the
FUSE session attempts the FUSE-over-io_uring transport by default
(`VFS_FUSE_URING`, bounded queue depth via `VFS_FUSE_URING_DEPTH`)
and falls back to the classic `/dev/fuse` read/write loop when io_uring
setup is unavailable or fails. The transport is a performance detail only:
request semantics, reply contents, cache invalidation, and teardown bounds
MUST be identical on both legs, and unmount MUST join transport threads on
both legs without leaking the mount.

### Overlay Base Reality

Overlay mounts scope reads to the configured base directory and write only
to the delta database (see the sandbox invariants above). Two consequences
are contractual:

1. Renaming a base-layer directory returns `EXDEV` rather than attempting a
   recursive copy-up; callers (e.g. `mv`) then perform an explicit
   copy+delete, which lands in the delta layer.
2. External mutation of the base tree under a live mount is detected where
   it matters: partial-origin reads validate the recorded base fingerprint
   and MUST fail rather than silently mix bytes from a drifted base file
   (see Partial-Origin Overlay Mode).

## Tool Calls

The tool call tracking schema captures tool invocations for debugging, auditing, and analysis.

### Schema

#### Table: `tool_calls`

Stores individual tool invocations with parameters and results. This is an insert-only audit log.

```sql
CREATE TABLE tool_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  parameters TEXT,
  result TEXT,
  error TEXT,
  started_at INTEGER NOT NULL,
  completed_at INTEGER NOT NULL,
  duration_ms INTEGER NOT NULL
)

CREATE INDEX idx_tool_calls_name ON tool_calls(name)
CREATE INDEX idx_tool_calls_started_at ON tool_calls(started_at)
```

**Fields:**

- `id` - Unique tool call identifier
- `name` - Tool name (e.g., 'read_file', 'web_search', 'execute_code')
- `parameters` - JSON-serialized input parameters (NULL if no parameters)
- `result` - JSON-serialized result (NULL if error)
- `error` - Error message (NULL if success)
- `started_at` - Invocation timestamp (Unix timestamp, seconds)
- `completed_at` - Completion timestamp (Unix timestamp, seconds)
- `duration_ms` - Execution duration in milliseconds

### Operations

#### Record Tool Call

```sql
INSERT INTO tool_calls (name, parameters, result, error, started_at, completed_at, duration_ms)
VALUES (?, ?, ?, ?, ?, ?, ?)
```

**Note:** Insert once when the tool call completes. Either `result` or `error` should be set, not both.

#### Query Tool Calls by Name

```sql
SELECT * FROM tool_calls
WHERE name = ?
ORDER BY started_at DESC
```

#### Query Recent Tool Calls

```sql
SELECT * FROM tool_calls
WHERE started_at > ?
ORDER BY started_at DESC
```

#### Analyze Tool Performance

```sql
SELECT
  name,
  COUNT(*) as total_calls,
  SUM(CASE WHEN error IS NULL THEN 1 ELSE 0 END) as successful,
  SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END) as failed,
  AVG(duration_ms) as avg_duration_ms
FROM tool_calls
GROUP BY name
ORDER BY total_calls DESC
```

### Consistency Rules

1. Exactly one of `result` or `error` SHOULD be non-NULL (mutual exclusion)
2. `completed_at` MUST always be set (no NULL values)
3. `duration_ms` MUST always be set and equal to `(completed_at - started_at) * 1000`
4. Parameters and results MUST be valid JSON strings when present
5. Records MUST NOT be updated or deleted (insert-only audit log)

### Implementation Notes

- This is an insert-only audit log - no updates or deletes
- Insert the record once when the tool call completes
- Set either `result` (on success) or `error` (on failure), but not both
- `parameters`, `result`, and `error` are stored as JSON-serialized strings
- `duration_ms` should be computed as `(completed_at - started_at) * 1000`
- Use indexes for efficient queries by name or time
- Consider periodic archival of old tool call records to a separate table

### Extension Points

Implementations MAY extend the tool call schema with additional functionality:

- Session/conversation grouping (add `session_id` field)
- User attribution (add `user_id` field)
- Cost tracking (add `cost` field for API calls)
- Parent/child relationships for nested tool calls
- Token usage tracking
- Input/output size metrics

Such extensions SHOULD use separate tables to maintain referential integrity.

## Virtual Filesystem

The virtual filesystem provides POSIX-like file operations for agent artifacts. The filesystem separates namespace (paths and names) from data (file content and metadata) using a Unix-like inode design. This enables hard links (multiple paths to the same file), efficient file operations, proper file metadata (permissions, timestamps), and chunked content storage.

### Schema

#### Table: `fs_config`

Stores filesystem-level configuration. This table is initialized once when the filesystem is created and MUST NOT be modified afterward.

```sql
CREATE TABLE fs_config (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
)
```

**Fields:**

- `key` - Configuration key
- `value` - Configuration value (stored as text)

**Required Configuration:**

| Key | Description | Default |
|-----|-------------|---------|
| `schema_version` | On-disk schema version | `0.8` |
| `chunk_size` | Size of data chunks in bytes | `65536` |
| `inline_threshold` | Maximum dense regular-file size stored inline in `fs_inode.data_inline` | `16384` |
| `history_epoch` | Identity of the current replay lineage | `1` |
| `history_valid` | Whether replay from the retained root and journal is valid | `1` |
| `history_floor_seq` | Lowest retained sequence boundary | `0` |

**Notes:**

- `chunk_size` determines the fixed size of data chunks in `fs_data`
- New v0.7+ filesystems use 64 KiB chunks by default; legacy databases retain their recorded chunk size until copy-migrated
- `inline_threshold` determines when dense regular files may avoid `fs_data` rows entirely
- Configuration is immutable after filesystem initialization
- Implementations MAY define additional configuration keys

#### Table: `fs_inode`

Stores file and directory metadata.

```sql
CREATE TABLE fs_inode (
  ino INTEGER PRIMARY KEY AUTOINCREMENT,
  mode INTEGER NOT NULL,
  nlink INTEGER NOT NULL DEFAULT 0,
  uid INTEGER NOT NULL DEFAULT 0,
  gid INTEGER NOT NULL DEFAULT 0,
  size INTEGER NOT NULL DEFAULT 0,
  atime INTEGER NOT NULL,
  mtime INTEGER NOT NULL,
  ctime INTEGER NOT NULL,
  rdev INTEGER NOT NULL DEFAULT 0,
  atime_nsec INTEGER NOT NULL DEFAULT 0,
  mtime_nsec INTEGER NOT NULL DEFAULT 0,
  ctime_nsec INTEGER NOT NULL DEFAULT 0,
  data_inline BLOB,
  storage_kind INTEGER NOT NULL DEFAULT 0
)
```

**Fields:**

- `ino` - Inode number (unique identifier)
- `mode` - File type and permissions (Unix mode bits)
- `nlink` - Number of hard links pointing to this inode
- `uid` - Owner user ID
- `gid` - Owner group ID
- `size` - Total file size in bytes
- `atime` - Last access time (Unix timestamp, seconds)
- `mtime` - Last modification time (Unix timestamp, seconds)
- `ctime` - Creation/change time (Unix timestamp, seconds)
- `rdev` - Device number for character and block devices (major/minor encoded)
- `atime_nsec` - Nanosecond component of last access time (0–999999999)
- `mtime_nsec` - Nanosecond component of last modification time (0–999999999)
- `ctime_nsec` - Nanosecond component of creation/change time (0–999999999)
- `data_inline` - Optional inline content for dense small regular files
- `storage_kind` - Storage layout marker: `0` for chunked data in `fs_data`, `1` for inline data in `data_inline`

**Storage Layout Rules:**

- Directories and symlinks MUST use `storage_kind = 0` and `data_inline IS NULL`
- Inline regular files MUST use `storage_kind = 1`, store all bytes in `data_inline`, and have no `fs_data` rows
- Chunked regular files MUST use `storage_kind = 0` and `data_inline IS NULL`
- `size` is authoritative for both layouts
- Inline files represent dense content only; sparse writes MUST transition to chunked storage
- Implementations MAY transition chunked files back to inline after truncation only when the resulting file is dense and at or below `inline_threshold`

**Mode Encoding:**

The `mode` field combines file type and permissions:

```
File type (upper bits):
  0o170000 - File type mask (S_IFMT)
  0o100000 - Regular file (S_IFREG)
  0o040000 - Directory (S_IFDIR)
  0o120000 - Symbolic link (S_IFLNK)
  0o010000 - FIFO/named pipe (S_IFIFO)
  0o020000 - Character device (S_IFCHR)
  0o060000 - Block device (S_IFBLK)
  0o140000 - Socket (S_IFSOCK)

Permissions (lower 12 bits):
  0o000777 - Permission bits (rwxrwxrwx)

Example:
  0o100644 - Regular file, rw-r--r--
  0o040755 - Directory, rwxr-xr-x
```

**Special Inodes:**

- Inode 1 MUST be the root directory

#### Table: `fs_dentry`

Maps names to inodes (directory entries).

```sql
CREATE TABLE fs_dentry (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  parent_ino INTEGER NOT NULL,
  ino INTEGER NOT NULL,
  UNIQUE(parent_ino, name)
)

CREATE INDEX idx_fs_dentry_parent ON fs_dentry(parent_ino, name)
```

**Fields:**

- `id` - Internal entry ID
- `name` - Basename (filename or directory name)
- `parent_ino` - Parent directory inode number
- `ino` - Inode this entry points to

**Constraints:**

- `UNIQUE(parent_ino, name)` - No duplicate names in a directory

**Notes:**

- Root directory (ino=1) has no dentry (no parent)
- Multiple dentries MAY point to the same inode (hard links)
- Link count is stored in `fs_inode.nlink` and must be incremented/decremented when dentries are added/removed

#### Table: `fs_data`

Maps each live file chunk to content in the shared chunk store. Chunk size is
configured at filesystem level via `fs_config`.

```sql
CREATE TABLE fs_data (
  ino INTEGER NOT NULL,
  chunk_index INTEGER NOT NULL,
  digest BLOB NOT NULL,
  PRIMARY KEY (ino, chunk_index)
)
```

**Fields:**

- `ino` - Inode number
- `chunk_index` - Zero-based chunk index (chunk 0 contains bytes 0 to chunk_size-1)
- `digest` - Raw 32-byte BLAKE3 digest identifying the chunk's `fs_chunk` row

**Notes:**

- Directories MUST NOT have data chunks
- Inline regular files MUST NOT have data chunks
- Chunk size is determined by the `chunk_size` value in `fs_config`
- New v0.7+ filesystems default to 64 KiB chunks
- All chunks except the last chunk of a dense chunked file SHOULD be exactly `chunk_size` bytes
- The last chunk MAY be smaller than `chunk_size`
- Sparse holes MAY be represented by missing chunk rows and MUST read back as zero bytes
- All-zero chunk rows MAY be omitted when doing so preserves read semantics
- Every `digest` MUST resolve to exactly one `fs_chunk` row
- Byte offset for a chunk = `chunk_index * chunk_size`
- To read at byte offset `N`: `chunk_index = N / chunk_size`, `offset_in_chunk = N % chunk_size`

#### Table: `fs_chunk`

Stores content-addressed chunk bytes shared by every `fs_data` mapping with
the same digest.

```sql
CREATE TABLE fs_chunk (
  digest BLOB PRIMARY KEY,
  data BLOB NOT NULL,
  refcount INTEGER NOT NULL DEFAULT 0
)
```

**Fields:**

- `digest` - Raw 32-byte BLAKE3 digest of `data`
- `data` - Binary chunk content, up to `chunk_size` bytes
- `refcount` - Number of live `fs_data` mappings that reference this digest

**Notes:**

- Equal chunk bytes MUST share one row.
- `refcount` counts live mappings only; journal retention is tracked
  separately by `fs_journal_chunk`.
- A zero-refcount row MAY remain while a retained journal entry references
  its digest. Garbage collection MUST remove it only after both the live
  mapping count and journal retention count reach zero.
- Digest computation and insertion MUST occur in the same transaction as the
  corresponding `fs_data` mapping change.

#### Table: `fs_symlink`

Stores symbolic link targets.

```sql
CREATE TABLE fs_symlink (
  ino INTEGER PRIMARY KEY,
  target TEXT NOT NULL
)
```

**Fields:**

- `ino` - Inode number of the symlink
- `target` - Target path (may be absolute or relative)

### Operations

#### Path Resolution

To resolve a path to an inode:

1. Start at root inode (ino=1)
2. Split path by `/` and filter empty components
3. For each component:
   ```sql
   SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?
   ```
4. Return final inode or NULL if any component not found

#### Creating a File

1. Resolve parent directory path to inode
2. Get chunk size from config:
   ```sql
   SELECT value FROM fs_config WHERE key = 'chunk_size'
   ```
3. Insert inode:
   ```sql
   INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
   VALUES (?, ?, ?, 0, ?, ?, ?)
   RETURNING ino
   ```
4. Insert directory entry:
   ```sql
   INSERT INTO fs_dentry (name, parent_ino, ino)
   VALUES (?, ?, ?)
   ```
5. Increment link count:
   ```sql
   UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?
   ```
6. If initial content is dense and `size <= inline_threshold`, store it inline:
   ```sql
   UPDATE fs_inode
   SET size = ?, data_inline = ?, storage_kind = 1, mtime = ?
   WHERE ino = ?
   ```
7. Otherwise, split data into chunks and content-address every stored chunk.
   Chunks that are entirely zero may be omitted — a missing mapping reads
   back as zeroes — but a writer that stores them pays almost nothing, since
   every zero chunk deduplicates to the one all-zero `fs_chunk` row:
   ```sql
   INSERT INTO fs_chunk (digest, data, refcount)
   VALUES (?, ?, 1)
   ON CONFLICT(digest) DO UPDATE SET refcount = refcount + 1;

   INSERT INTO fs_data (ino, chunk_index, digest)
   VALUES (?, ?, ?)
   ```
   Where `digest` is the raw 32-byte BLAKE3 digest of `data`, and
   `chunk_index` starts at 0 and increments for each logical chunk.
8. Update inode size and mark chunked storage:
   ```sql
   UPDATE fs_inode SET size = ?, data_inline = NULL, storage_kind = 0, mtime = ? WHERE ino = ?
   ```

#### Reading a File

1. Resolve path to inode
2. Fetch inode size and storage layout:
   ```sql
   SELECT size, storage_kind, data_inline FROM fs_inode WHERE ino = ?
   ```
3. If `storage_kind = 1`, return `data_inline` truncated to `size`
4. Otherwise, fetch all chunks in order:
   ```sql
   SELECT d.chunk_index, c.data
   FROM fs_data d
   JOIN fs_chunk c ON c.digest = d.digest
   WHERE d.ino = ?
   ORDER BY d.chunk_index ASC
   ```
5. Concatenate chunks in order, treating missing sparse chunks as zeroes up to `size`
6. Update access time:
   ```sql
   UPDATE fs_inode SET atime = ? WHERE ino = ?
   ```

#### Reading a File at Offset

To read `length` bytes starting at byte offset `offset`:

1. Resolve path to inode
2. Fetch inode size and storage layout:
   ```sql
   SELECT size, storage_kind, data_inline FROM fs_inode WHERE ino = ?
   ```
3. If `storage_kind = 1`, slice `data_inline` according to `offset` and `length`
4. Otherwise, get chunk size from config:
   ```sql
   SELECT value FROM fs_config WHERE key = 'chunk_size'
   ```
5. Calculate chunk range:
   - `start_chunk = offset / chunk_size`
   - `end_chunk = (offset + length - 1) / chunk_size`
6. Fetch required chunks:
   ```sql
   SELECT d.chunk_index, c.data
   FROM fs_data d
   JOIN fs_chunk c ON c.digest = d.digest
   WHERE d.ino = ? AND d.chunk_index >= ? AND d.chunk_index <= ?
   ORDER BY d.chunk_index ASC
   ```
7. Extract the requested byte range from the chunks:
   - `offset_in_first_chunk = offset % chunk_size`
   - Skip first `offset_in_first_chunk` bytes of first chunk
   - Take `length` total bytes across chunks
   - Fill missing sparse chunks with zeroes up to EOF

#### Listing a Directory

1. Resolve directory path to inode
2. Query entries:
   ```sql
   SELECT name FROM fs_dentry WHERE parent_ino = ? ORDER BY name ASC
   ```

#### Deleting a File

1. Resolve path to get inode and parent
2. Delete directory entry:
   ```sql
   DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?
   ```
3. Decrement link count:
   ```sql
   UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?
   ```
4. Check if last link:
   ```sql
   SELECT nlink FROM fs_inode WHERE ino = ?
   ```
5. If nlink = 0, delete the inode and its mappings, decrementing the
   corresponding `fs_chunk.refcount` values in the same transaction:
   ```sql
   DELETE FROM fs_inode WHERE ino = ?
   DELETE FROM fs_data WHERE ino = ?
   ```
6. Garbage-collect zero-refcount `fs_chunk` rows only when they are not pinned
   by `fs_journal_chunk`.

#### Creating a Hard Link

1. Resolve source path to get inode
2. Resolve destination parent to get parent_ino
3. Insert new directory entry:
   ```sql
   INSERT INTO fs_dentry (name, parent_ino, ino)
   VALUES (?, ?, ?)
   ```
4. Increment link count:
   ```sql
   UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?
   ```

#### Reading File Metadata (stat)

1. Resolve path to inode
2. Query inode (includes link count):
   ```sql
   SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
          atime_nsec, mtime_nsec, ctime_nsec
   FROM fs_inode WHERE ino = ?
   ```

### Initialization

When creating a new agent database, initialize the filesystem configuration and root directory:

```sql
-- Initialize filesystem configuration
INSERT INTO fs_config (key, value) VALUES ('schema_version', '0.8');
INSERT INTO fs_config (key, value) VALUES ('chunk_size', '65536');
INSERT INTO fs_config (key, value) VALUES ('inline_threshold', '16384');

-- Initialize root directory
INSERT INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime)
VALUES (1, 16877, 1, 0, 0, 0, unixepoch(), unixepoch(), unixepoch());
```

Where `16877` = `0o040755` (directory with rwxr-xr-x permissions)

**Note:** The `chunk_size` and `inline_threshold` values can be customized at filesystem creation time but MUST NOT be changed afterward. The root directory has `nlink=1` as it has no parent directory entry.

### Schema Migration

Migrations are keyed by `PRAGMA user_version` and land any supported old
schema (v0.0, v0.2, v0.4, v0.5, v0.6) at the current version with one command:

```bash
vfs migrate <id-or-path>
```

In-place migration requirements:

1. Every migration step MUST be idempotent and applied inside one IMMEDIATE transaction that stamps `PRAGMA user_version` before committing.
2. Existing file contents MUST keep their recorded `chunk_size`; the in-place path does not re-chunk data. A defaulted `inline_threshold` is `min(16384, chunk_size)`.
3. Open paths (mount, fs, exec, SDK open) MUST NOT run version upgrades implicitly; they reject old schemas and direct the user to `vfs migrate`.
4. The v0.5 → v0.6 migration creates `fs_session_metadata` and preserves all existing filesystem, overlay, key-value, and tool-call rows.
5. The v0.6 → v0.7 migration hashes every existing `fs_data.data` blob,
   deduplicates it into `fs_chunk`, replaces `fs_data.data` with the raw
   32-byte digest mapping, and initializes the thin journal schema in the same
   transaction.

The copy-based mode rebuilds the database with the current chunk layout:

```bash
vfs migrate <source-old.db> --copy <target.db> --verify
```

Copy-migration requirements:

1. The source database MUST NOT be modified in place.
2. The target database MUST be newly created unless an explicit overwrite option is used.
3. The migration MUST preserve inode numbers, dentries, symlinks, KV rows, tool-call rows, overlay whiteouts, overlay origin mappings, overlay configuration, and session metadata.
4. Small dense regular files MAY be converted to inline storage.
5. Chunked files MUST be re-chunked using the target `chunk_size`.
6. Each non-zero target chunk MUST be hashed into `fs_chunk`; identical
   content MUST deduplicate and `refcount` MUST equal its live mapping count.
7. Sparse holes MUST preserve read-back semantics.
8. Verification MUST run integrity checks and compare source/target metadata and file contents.
9. After checkpointing the target, copying only the main `.db` file MUST be sufficient to reopen and verify the target state.

### Consistency Rules

1. Root inode (ino=1) MUST always exist
2. Every dentry MUST reference a valid inode
3. Every dentry MUST reference a valid parent inode
4. No directory MAY contain duplicate names
5. Directories MUST have mode with S_IFDIR bit set
6. Regular files MUST have mode with S_IFREG bit set
7. Inline regular files MUST have `storage_kind = 1`, `data_inline` length equal to `size`, and no `fs_data` rows
8. Chunked regular files MUST have `storage_kind = 0` and `data_inline IS NULL`
9. File reads MUST return exactly `size` bytes regardless of sparse missing chunks
10. Every inode MUST have at least one dentry (except root)
11. Every `fs_data.digest` and `fs_chunk.digest` MUST be exactly 32 bytes
12. Every `fs_data.digest` MUST resolve to an `fs_chunk` row
13. Every `fs_chunk.refcount` MUST equal the number of live `fs_data` mappings for that digest

### Implementation Notes

- Use `RETURNING` clause to safely get auto-generated inode numbers
- Parent directories are created implicitly as needed
- Empty files have an inode but no data chunks
- Symlink resolution is implementation-defined (not part of schema)
- Use transactions for multi-step operations to maintain consistency

### Extension Points

Implementations MAY extend the filesystem schema with additional functionality:

- Extended attributes table
- File ACLs and advanced permissions
- Quota tracking per user/group
- Version history and snapshots
- Content deduplication
- Compression metadata
- File checksums/hashes

Such extensions SHOULD use separate tables to maintain referential integrity.

## Overlay Filesystem

The overlay filesystem provides copy-on-write semantics by layering a writable delta filesystem on top of a read-only base filesystem. Changes are written to the delta layer while the base layer remains unmodified. This enables sandboxed execution where modifications can be discarded or committed independently.

### Whiteouts

When a file is deleted from an overlay filesystem, the deletion must be recorded so that lookups do not fall through to the base layer. This is accomplished using "whiteouts" - markers that indicate a path has been explicitly deleted.

#### Table: `fs_whiteout`

Tracks deleted paths in the overlay to prevent base layer visibility.

```sql
CREATE TABLE fs_whiteout (
  path TEXT PRIMARY KEY,
  parent_path TEXT NOT NULL,
  created_at INTEGER NOT NULL
)

CREATE INDEX idx_fs_whiteout_parent ON fs_whiteout(parent_path)
```

**Fields:**

- `path` - Normalized absolute path that has been deleted
- `parent_path` - Parent directory path (for efficient child lookups)
- `created_at` - Deletion timestamp (Unix timestamp, seconds)

**Notes:**

- The `parent_path` column enables O(1) lookups of whiteouts within a directory, avoiding expensive `LIKE` pattern matching
- For the root directory `/`, `parent_path` is `/`
- For other paths, `parent_path` is the path with the final component removed (e.g., `/foo/bar` has parent `/foo`)

### Overlay Configuration

Overlay databases persist the base layer they were initialized with so an existing database can be reopened with the same overlay semantics.

#### Table: `fs_overlay_config`

```sql
CREATE TABLE fs_overlay_config (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
)
```

**Required Configuration:**

| Key | Description |
|-----|-------------|
| `base_path` | Canonical path to the read-only base directory |

**Optional Configuration:**

| Key | Description |
|-----|-------------|
| `parent_artifact` | sha256 (64 lowercase hex chars) of the frozen parent artifact a branch delta reads through |

Copy migration MUST preserve this table when migrating an overlay delta database. Without it, a migrated overlay database would mount as a plain Vfs database and lose base-layer visibility.

### Branch Deltas and the Artifact Store

`vfs branch` forks a run session by snapshotting the parent database
(`VACUUM INTO` through a read transaction, drained first, so a live parent
is never stopped), publishing the snapshot as an immutable content-addressed
artifact at `~/.vfs/artifacts/<sha256>.db` (chmod `0444`, published by
same-filesystem rename after the bytes are durable), and creating a new
session whose delta records the artifact digest under `parent_artifact`.
Because the store is content-addressed, branches taken at the same parent
state share one artifact.

A delta carrying `parent_artifact` mounts as a stacked overlay:

```
overlay(branch delta, overlay(parent artifact, ... , host base))
```

Every mount surface resolves the chain the same way (one shared stack
builder). Chain semantics:

- Parent artifacts are opened strictly read-only; their `finalize`/drain are
  no-ops and nothing in-process may hold a writable handle to the store.
- Before opening, each artifact is re-hashed and compared against the digest
  recorded by its child. A missing or drifted artifact refuses the mount
  (invariant: the branched state is reproduced exactly, or not at all).
  `vfs run` reports this refusal with the invalid-session exit status `5`.
- Chains recurse (a parent may itself record `parent_artifact`) up to a
  depth of 8, refused beyond that.
- Partial-origin copy-up is forced Off on branch mounts: partial-origin rows
  fingerprint real base files, and a branch's base is a virtual stack.
- Packed artifacts never carry `parent_artifact`: `vfs pack` materializes the
  parent chain into the staged database, so the handoff wire contract and
  `artifactVersion` are unchanged by branching.
- Store GC (`vfs prune artifacts`) deletes only artifacts unreachable from
  any installed session's chain. Reachability is computed conservatively —
  inactive sessions read under the exclusive session lock, live sessions
  answered over the control socket, anything unclassifiable aborts the
  prune — and an advisory lock on the store directory serializes GC against
  in-flight branch publication (a fork holds it shared from artifact install
  until the referencing session is installed).

### Operations

#### Create Whiteout

When deleting a file that exists in the base layer:

```sql
INSERT INTO fs_whiteout (path, parent_path, created_at)
VALUES (?, ?, ?)
ON CONFLICT(path) DO UPDATE SET created_at = excluded.created_at
```

#### Check for Whiteout

Before falling through to the base layer during lookup:

```sql
SELECT 1 FROM fs_whiteout WHERE path = ?
```

#### Remove Whiteout

When creating a file at a previously deleted path:

```sql
DELETE FROM fs_whiteout WHERE path = ?
```

#### List Child Whiteouts

When listing a directory, get whiteouts to exclude from base layer results:

```sql
SELECT path FROM fs_whiteout WHERE parent_path = ?
```

### Overlay Lookup Semantics

1. Check if path exists in delta layer → return delta entry
2. Check if path has a whiteout → return "not found"
3. Check if path exists in base layer → return base entry
4. Return "not found"

### Inode Origin Tracking

When a file is copied from the base layer to the delta layer during a copy-up operation (e.g., when creating a hard link to a base file), the original base inode number must be preserved. This is necessary because the kernel caches inode numbers, and returning a different inode after copy-up causes ENOENT errors or cache inconsistencies.

This mechanism is similar to Linux overlayfs's `trusted.overlay.origin` extended attribute, which stores a file handle to the lower inode.

#### Table: `fs_origin`

Maps delta layer inodes to their original base layer inodes.

```sql
CREATE TABLE fs_origin (
  delta_ino INTEGER PRIMARY KEY,
  base_ino INTEGER NOT NULL
)
```

**Fields:**

- `delta_ino` - Inode number in the delta layer
- `base_ino` - Original inode number from the base layer

#### Operations

##### Store Origin Mapping

When copying a file from base to delta during copy-up:

```sql
INSERT OR REPLACE INTO fs_origin (delta_ino, base_ino)
VALUES (?, ?)
```

##### Get Origin Inode

When stat'ing a file that exists in delta, check if it has an origin:

```sql
SELECT base_ino FROM fs_origin WHERE delta_ino = ?
```

If a mapping exists, return `base_ino` instead of `delta_ino` in stat results.

### Partial-Origin Overlay Mode

Partial-origin copy-up is an opt-in overlay mode selected by the first-class
CLI policy `--partial-origin <off|on|auto>` (with
`--partial-origin-threshold-bytes` sizing the `auto` cutoff). The default
overlay behavior remains whole-file copy-up (`off`). In opt-in mode,
write-opening a regular base-layer file creates a delta inode with the
original size and metadata, records the base path/fingerprint in
`fs_partial_origin`, and stores only changed chunk mappings in `fs_data` plus
`fs_chunk_override`; their bytes live in `fs_chunk`. Reads merge changed
chunks from the delta layer with unchanged chunks from the base layer.

The base fallback is part of the file's integrity contract. Implementations MUST
fail reads of partial-origin files if the recorded base size or modification
metadata no longer matches the current base file. Snapshot/restore of the main
delta database is supported only when the same unchanged base path is available.
A database containing partial-origin rows is not portable on its own:
`vfs backup` rejects it unless `--materialize` folds the base bytes in,
`vfs materialize` produces a portable copy, and `vfs integrity`
exposes the dependency via `--require-portable` and `--check-base`.

#### Tables: `fs_partial_origin` and `fs_chunk_override`

```sql
CREATE TABLE fs_partial_origin (
  delta_ino INTEGER PRIMARY KEY,
  base_ino INTEGER NOT NULL,
  base_path TEXT NOT NULL,
  base_size INTEGER NOT NULL,
  base_fingerprint_size INTEGER NOT NULL DEFAULT -1,
  base_mtime INTEGER NOT NULL DEFAULT 0,
  base_mtime_nsec INTEGER NOT NULL DEFAULT 0,
  base_ctime INTEGER NOT NULL DEFAULT 0,
  base_ctime_nsec INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
)

CREATE TABLE fs_chunk_override (
  delta_ino INTEGER NOT NULL,
  chunk_index INTEGER NOT NULL,
  PRIMARY KEY (delta_ino, chunk_index)
)
```

Partial-origin stays opt-in. Coverage pinning the mode includes remount,
main-DB snapshot restore, unlink cleanup/whiteout behavior, hardlink survival,
rename plus `readdir_plus`, truncate shrink/extend, and base drift detection.
It SHOULD NOT become the default until the FUSE/CLI torture and POSIX gates
pass with the policy enabled.

### Consistency Rules

1. A whiteout MUST be removed when a new file is created at that path
2. A whiteout MUST be created when deleting a file that exists in the base layer
3. The `parent_path` MUST be correctly derived from `path`
4. Whiteouts only affect overlay lookups, not the underlying base filesystem
5. When copying a file from base to delta, the origin mapping MUST be stored
6. When stat'ing a delta file with an origin mapping, the base inode MUST be returned
7. Existing overlay databases with legacy `fs_whiteout(path, created_at)` rows MUST synthesize `parent_path` before using the v0.5 whiteout schema
8. Partial-origin files MUST remove `fs_partial_origin`, `fs_chunk_override`, and `fs_origin` rows when the last delta link is unlinked

## Session Handoff Metadata

Transfer preparation state is stored inside the database so a packed
`delta.db` carries its own double-resume guard and future seed provenance.

### Table: `fs_session_metadata`

```sql
CREATE TABLE fs_session_metadata (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
)
```

**Defined keys:**

| Key | Encoding | Description |
|-----|----------|-------------|
| `generation` | Base-10 unsigned integer text | Monotonic counter incremented by each successful `vfs pack` |
| `seeded_paths` | JSON string array | Paths materialized by the future seed operation; defaults to `[]` |
| `seed_pin` | Full git commit hash text | Base provenance recorded by `vfs seed`; `vfs adopt` verifies the receiving base checkout's `HEAD` against it |

`vfs pack` updates metadata only in its private database copy. The new
generation becomes authoritative when the compacted copy is atomically
published as the session's `delta.db`. A failed pack MUST leave both metadata
keys and all filesystem rows at their pre-pack values.

## Operation Journal

The v0.8 journal is an ordered row-delta stream. Each row identifies one live
filesystem table row that was inserted, replaced, or deleted by a committed
SQLite transaction. Replaying a root snapshot followed by complete transaction
groups reconstructs the live filesystem state without interpreting
operation-specific payloads.

### Table: `fs_op_journal`

```sql
CREATE TABLE fs_op_journal (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  txn_id INTEGER NOT NULL,
  label TEXT NOT NULL,
  tbl TEXT NOT NULL,
  verb TEXT NOT NULL,
  row TEXT NOT NULL,
  wallclock_ms INTEGER NOT NULL
)

CREATE INDEX idx_fs_op_journal_txn_id ON fs_op_journal(txn_id)
```

**Fields:**

- `seq` - Monotonic journal sequence number and total operation order
- `txn_id` - Identifier shared by every row delta committed in one
  SQLite transaction; it equals the `seq` of the group's first row
- `label` - Diagnostic operation label such as `write`, `rename`, or `reap`;
  replay does not branch on this value
- `tbl` - One of `fs_inode`, `fs_dentry`, `fs_data`, `fs_symlink`,
  `fs_whiteout`, `fs_origin`, `fs_partial_origin`, `fs_chunk_override`, or
  `fs_overlay_config`
- `verb` - `upsert` for a complete post-image or `delete` for a primary-key
  tombstone
- `row` - JSON object containing the complete replayable row shape. Binary
  content is represented by lowercase hexadecimal BLAKE3 digests, never
  embedded bytes. An `fs_inode` upsert carries every inode column except raw
  `data_inline`; inline bytes are normalized to `data_inline_digest`.
  `fs_dentry` upserts include the stable dentry `id` as well as
  `(parent_ino,name,ino)`.
- `wallclock_ms` - Commit-associated Unix epoch timestamp in milliseconds

One logical mutation may produce several row deltas. Deltas from one commit
MUST share one `txn_id`, and journal rows MUST commit atomically with the live
rows they describe. Consumers replay commits in `seq` order and MUST treat all
rows with one `txn_id` as a unit. Writers construct post-images from mutation
inputs and mutation-statement results inside the transaction; they MUST NOT
issue follow-up reads merely to build journal rows.

Transaction IDs need no separate allocator state: a group's `txn_id` is the
`seq` AUTOINCREMENT assigns to its first row. Writers maintain an optimistic
next-sequence hint, verify it against the assigned first `seq`, and repair a
stale hint inside the same exclusive transaction. A cold writer finds the
head with `ORDER BY seq DESC LIMIT 1`; it does not run an aggregate on the hot
commit path.

### Table: `fs_journal_chunk`

```sql
CREATE TABLE fs_journal_chunk (
  seq INTEGER NOT NULL,
  digest BLOB NOT NULL
)

CREATE INDEX idx_fs_journal_chunk_digest ON fs_journal_chunk(digest)
```

Each row records that the operation at `seq` retains a raw 32-byte BLAKE3
digest. It is a retention relation, not a second live-mapping refcount:
`fs_chunk.refcount` continues to equal the number of `fs_data` mappings.
Collecting a journal range MUST remove its `fs_journal_chunk` rows in the same
transaction; an `fs_chunk` row is eligible for garbage collection only when
its live refcount is zero and neither a retained journal row nor a root
snapshot names its digest.

## Root Snapshots

A root snapshot is an immutable relational copy of replayable filesystem
state at a sequence boundary.

```sql
CREATE TABLE fs_snapshot (
  snapshot_id INTEGER PRIMARY KEY AUTOINCREMENT,
  through_seq INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL,
  reason TEXT NOT NULL,
  history_epoch INTEGER NOT NULL,
  UNIQUE(history_epoch, through_seq)
)
```

Snapshot row tables mirror the replayable live tables and prefix their keys
with `snapshot_id`:

- `fs_snapshot_inode`
- `fs_snapshot_dentry`
- `fs_snapshot_data`
- `fs_snapshot_symlink`
- `fs_snapshot_whiteout`
- `fs_snapshot_origin`
- `fs_snapshot_partial_origin`
- `fs_snapshot_chunk_override`

`fs_snapshot_inode` stores `data_inline_digest` rather than raw inline bytes.
`fs_snapshot_chunk(snapshot_id, digest)` pins every digest needed by snapshot
inode and data rows. `fs_snapshot_meta(snapshot_id, key, value)` captures
replay provenance including `seed_pin`, `seeded_paths`, and
`parent_artifact`. Snapshot capture runs as direct SQL over the live tables
inside one transaction; it does not hydrate file contents through runtime
filesystem read APIs.

A new database starts with an `init` snapshot at epoch 1 through sequence 0.
Migration to v0.8 discards the non-replayable v0.7 journal and its pins,
creates a `migrate` snapshot at epoch 1 through sequence 0, and sets
`history_epoch=1`, `history_valid=1`, and `history_floor_seq=0`. In-place and
copy migration establish the same history boundary.

## History Range, Epochs, and Reconstruction

`history_floor_seq` and the current journal/snapshot head define the inclusive
advertised range. A target is replayable only when all of the following hold:

1. `history_valid=1`;
2. the target is within `[history_floor_seq, history_head_seq]`;
3. the target equals the final `seq` of a complete transaction group, or is
   exactly the floor root;
4. a current-epoch snapshot exists at or before the target; and
5. journal rows after that snapshot through the target are contiguous and
   contain only whole transaction groups.

Consumers MUST reject targets outside the range, targets inside a transaction,
invalid epochs, missing roots, and journal gaps. They MUST NOT silently choose
the nearest available state.

Reconstruction operates only on a private writable staging database. It loads
the nearest current-epoch root at or before the target into isolated replay
state, applies complete row-delta groups in ascending `seq`, then atomically
replaces these live tables:

- `fs_inode`
- `fs_dentry`
- `fs_data`
- `fs_symlink`
- `fs_whiteout`
- `fs_origin`
- `fs_partial_origin`
- `fs_chunk_override`

For `fs_overlay_config`, replay replaces only `parent_artifact`; `base_path`
belongs to the receiving staging database and MUST remain unchanged. Snapshot
provenance restores `seed_pin` and `seeded_paths`. `kv_store`, `tool_calls`,
and session `generation` are outside filesystem-history scope and MUST remain
untouched.

Inline inode bytes are rematerialized from `fs_chunk` through
`data_inline_digest`. Reconstruction MUST fail if any inline or `fs_data`
digest is missing. After replay it removes future journal rows and snapshots,
rebuilds the journal AUTOINCREMENT table so the next group begins at
`target+1`, recomputes every chunk refcount from live `fs_data`, and collects
only chunks satisfying the three-way rule:

```text
refcount == 0
AND no fs_journal_chunk pin
AND no fs_snapshot_chunk pin
```

The inode allocator MUST remain above every inode that appeared anywhere in
the retained pre-reconstruction live state, snapshots, or journal, so a
reverted lineage never reuses an inode identity. Before publication,
reconstruction MUST pass current-schema integrity checks and a visible-tree
walk from inode 1.

### Durable epoch invalidation

The journaling kill switch is recorded durably, not inferred from the current
environment:

- the first writable open with journaling disabled changes
  `history_valid` from `1` to `0`;
- later disabled opens do not rewrite the marker;
- read-only opens never change history markers;
- the first writable open after journaling is re-enabled increments
  `history_epoch`, removes the stale journal, pins, and snapshots, captures an
  `epoch` root at the current state, publishes that root as the new floor, and
  sets `history_valid=1`.

No target from an invalidated or prior epoch remains available after
revalidation.

### Snapshot-covered retention and pack floors

Journal collection computes the sequence horizon as
`history_head_seq - VFS_JOURNAL_RETENTION_OPS` and chooses the greatest
complete transaction boundary at or below it. If that boundary advances the
floor, collection reconstructs state at the boundary in isolated replay
state, captures a new `gc` root there, publishes the new floor, then removes
journal groups through the boundary and older/superseded roots in one
transaction. Therefore every advertised target always has a covering root and
a contiguous journal suffix.

`vfs pack` is a generation boundary. On its private staging database it first
materializes any branch-parent chain and applies configured prunes, then
captures a `pack` root at the current head, removes all older journal targets
and snapshots, and sets that root as the fresh floor. Pre-pack targets are
intentionally unavailable from the packed generation.

## Key-Value Data

The key-value store provides simple get/set operations for agent context and state.

### Schema

#### Table: `kv_store`

Stores arbitrary key-value pairs with automatic timestamping.

```sql
CREATE TABLE kv_store (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  created_at INTEGER DEFAULT (unixepoch()),
  updated_at INTEGER DEFAULT (unixepoch())
)

CREATE INDEX idx_kv_store_created_at ON kv_store(created_at)
```

**Fields:**

- `key` - Unique key identifier
- `value` - JSON-serialized value
- `created_at` - Creation timestamp (Unix timestamp, seconds)
- `updated_at` - Last update timestamp (Unix timestamp, seconds)

### Operations

#### Set a Value

```sql
INSERT INTO kv_store (key, value, updated_at)
VALUES (?, ?, unixepoch())
ON CONFLICT(key) DO UPDATE SET
  value = excluded.value,
  updated_at = unixepoch()
```

#### Get a Value

```sql
SELECT value FROM kv_store WHERE key = ?
```

#### Delete a Value

```sql
DELETE FROM kv_store WHERE key = ?
```

#### List All Keys

```sql
SELECT key, created_at, updated_at FROM kv_store ORDER BY key ASC
```

### Consistency Rules

1. Keys MUST be unique (enforced by PRIMARY KEY)
2. Values MUST be valid JSON strings
3. Timestamps MUST use Unix epoch format (seconds)

### Implementation Notes

- Values are stored as JSON strings; serialize before storing, deserialize after retrieving
- Use `ON CONFLICT` clause for upsert operations
- Indexes on `created_at` support temporal queries
- Updates automatically refresh the `updated_at` timestamp
- Keys can use any naming convention (e.g., namespaced: `user:preferences`, `session:state`)

### Extension Points

Implementations MAY extend the key-value store schema with additional functionality:

- Namespaced keys with hierarchy support
- Value versioning/history
- TTL (time-to-live) for automatic expiration
- Value size limits and quotas

Such extensions SHOULD use separate tables to maintain referential integrity.

## Revision History

### Version 0.8

- Replaced operation-specific journal payloads with replayable row-delta
  post-images grouped atomically by `txn_id`
- Added immutable root snapshots for inode, namespace, content mapping,
  symlink, overlay, and provenance state
- Normalized inline bytes to content-addressed digests in both journal and
  snapshot rows, with explicit snapshot and journal pins
- Added history epoch, validity, and floor markers
- The v0.7 → v0.8 migration discards the old non-replayable journal and
  establishes one migration root at epoch 1 through sequence 0

### Version 0.7

- Replaced inline chunk blobs in `fs_data` with raw 32-byte BLAKE3 digest
  mappings into the deduplicated `fs_chunk` content-addressed store
- Added exact live-mapping refcounts and journal-aware retention for
  zero-refcount chunk rows
- Added the thin logical-operation journal: one row per operation,
  transaction grouping, digest-only chunk references, and explicit retention
  state
- The v0.6 → v0.7 migration preserves byte-identical file contents while
  deduplicating equal chunks; copy migration additionally re-chunks into the
  current 64 KiB layout

### Version 0.6

- Added `fs_session_metadata(key, value)` for persistent handoff state
- `vfs pack` increments the `generation` key and reads `seeded_paths` into its manifest
- The v0.5 → v0.6 migration is additive and creates the metadata table in the same schema transaction
- Copy migration preserves session metadata when present

### Version 0.5

- Default chunk size raised to 64 KiB for new filesystems (`chunk_size` in `fs_config`)
- Added inline storage for dense regular files at or below `inline_threshold` (current default 16 KiB): `data_inline` and `storage_kind` columns on `fs_inode`, with layout rules and consistency checks
- Added `inline_threshold` to the required `fs_config` keys
- Added partial-origin overlay mode tables (`fs_partial_origin`, `fs_chunk_override`) behind the opt-in `--partial-origin` CLI policy
- Whiteout schema requires `parent_path`; legacy `fs_whiteout(path, created_at)` rows are synthesized on migration
- Schema migrations are keyed by `PRAGMA user_version`; `vfs migrate` lands
  any supported old schema at the current version in place, and `--copy`
  rebuilds with the current chunk layout

### Version 0.4

- Added nanosecond timestamp precision for `atime`, `mtime`, and `ctime`
- Added `atime_nsec`, `mtime_nsec`, `ctime_nsec` columns to `fs_inode` table (DEFAULT 0 for backward compatibility)
- Nanosecond precision enables correct NFS `wcc_data` cache invalidation when multiple operations occur within the same second
- Added POSIX special file support (FIFOs, character devices, block devices, sockets)
- Added `rdev` column to `fs_inode` table for device major/minor numbers
- Added `S_IFIFO`, `S_IFCHR`, `S_IFBLK`, `S_IFSOCK` file type constants to Mode Encoding
- Updated stat query to include `rdev` field

### Version 0.3

- Added `fs_origin` table to Overlay Filesystem for tracking copy-up origin inodes
- Origin tracking ensures consistent inode numbers after copy-up (similar to Linux overlayfs `trusted.overlay.origin`)

### Version 0.2

- Added Overlay Filesystem section with `fs_whiteout` table for copy-on-write semantics
- Whiteout table includes `parent_path` column with index for efficient O(1) child lookups
- Added `nlink` column to `fs_inode` table to store link count directly
- Link count is now maintained in the inode rather than computed via COUNT(*) on `fs_dentry`

### Version 0.1

- Added `fs_config` table for filesystem-level configuration
- Changed `fs_data` table to use fixed-size chunks with `chunk_index` instead of variable-size chunks with `offset` and `size`
- Added `chunk_size` configuration option (default: 4096 bytes)
- Added "Reading a File at Offset" operation for efficient partial reads
- Chunk-based storage enables efficient random access reads without loading entire files

### Version 0.0

- Initial specification
- Tool call audit trail (`tool_calls` table)
- Virtual filesystem (`fs_inode`, `fs_dentry`, `fs_data`, `fs_symlink` tables)
- Key-value store (`kv_store` table)
