//! Error types for the Vfs SDK.

use thiserror::Error;

/// The main error type for the Vfs SDK.
///
/// Wrapper variants chain their cause through `source()` only and keep it out
/// of `Display`: `#[from]` already exposes the inner error to reporters that
/// walk the chain (anyhow `{:#}`), so repeating `{0}` in the message would
/// print every cause twice ("database error: X: X").
#[derive(Debug, Error)]
pub enum Error {
    /// Database error from turso
    #[error("database error")]
    Database(#[from] turso::Error),

    /// IO error
    #[error("io error")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error
    #[error("json error")]
    Json(#[from] serde_json::Error),

    /// System time error
    #[error("time error")]
    Time(#[from] std::time::SystemTimeError),

    /// Filesystem-specific error with errno semantics
    #[error(transparent)]
    Fs(#[from] crate::fs::FsError),

    /// Invalid agent ID format
    #[error("invalid agent ID '{0}': agent IDs must contain only alphanumeric characters, hyphens, and underscores")]
    InvalidAgentId(String),

    /// Agent not found
    #[error("agent '{id}' not found at '{path}'")]
    AgentNotFound { id: String, path: String },

    /// Database file path does not exist
    #[error("database not found: {0}")]
    DatabaseNotFound(String),

    /// Invalid path encoding
    #[error("path '{0}' is not valid UTF-8")]
    InvalidUtf8Path(String),

    /// Base directory does not exist
    #[error("base directory does not exist: {0}")]
    BaseDirectoryNotFound(String),

    /// Path is not a directory
    #[error("path is not a directory: {0}")]
    NotADirectory(String),

    /// Tool call not found
    #[error("tool call not found")]
    ToolCallNotFound,

    /// Connection pool timeout - no connections available
    #[error("connection pool timeout: no connections available")]
    ConnectionPoolTimeout,

    /// Invalid encryption key
    #[error("invalid encryption key: {0}")]
    InvalidEncryptionKey(String),

    /// Internal error (for unexpected conditions)
    #[error("{0}")]
    Internal(String),

    /// Schema version mismatch - database schema version doesn't match expected version
    #[error("schema version mismatch: database is version {found}, expected {expected}")]
    SchemaVersionMismatch { found: String, expected: String },

    /// A hollow database contains metadata but not its content-addressed chunk bytes.
    #[error(
        "database is a remote metadata artifact whose chunk bytes are not present; hydrate it before opening writable"
    )]
    ChunksHollow,

    /// A chunk source returned bytes that do not match the requested digest.
    #[error("hydrated chunk {digest} does not match its BLAKE3 digest")]
    ChunkDigestMismatch { digest: String },

    /// A stored chunk digest cannot identify a BLAKE3 object.
    #[error("stored chunk digest has length {length}, expected 32 bytes")]
    InvalidChunkDigest { length: usize },

    /// Durable history markers record a journaling gap.
    #[error(
        "filesystem history epoch {epoch} is not replayable (available range {floor_seq}..={head_seq})"
    )]
    HistoryInvalid {
        epoch: i64,
        floor_seq: i64,
        head_seq: i64,
    },

    /// A requested history target is outside the retained range.
    #[error(
        "history target {target_seq} is outside the available range {floor_seq}..={head_seq} in epoch {epoch}"
    )]
    HistoryTargetOutOfRange {
        target_seq: i64,
        floor_seq: i64,
        head_seq: i64,
        epoch: i64,
    },

    /// A requested sequence would expose only part of a committed mutation.
    #[error(
        "history target {target_seq} is inside transaction {txn_id}; use complete target {transaction_end_seq} (available range {floor_seq}..={head_seq}, epoch {epoch})"
    )]
    HistoryTargetMidTransaction {
        target_seq: i64,
        txn_id: i64,
        transaction_end_seq: i64,
        floor_seq: i64,
        head_seq: i64,
        epoch: i64,
    },

    /// No root snapshot can seed replay to the requested target.
    #[error(
        "history target {target_seq} has no covering root snapshot in epoch {epoch} (available range {floor_seq}..={head_seq})"
    )]
    HistorySnapshotMissing {
        target_seq: i64,
        floor_seq: i64,
        head_seq: i64,
        epoch: i64,
    },

    /// The retained journal is not contiguous after its covering snapshot.
    #[error(
        "history target {target_seq} has a journal gap after root {snapshot_seq}: expected seq {expected_seq}, found {found_seq:?} (available range {floor_seq}..={head_seq}, epoch {epoch})"
    )]
    HistoryGap {
        target_seq: i64,
        snapshot_seq: i64,
        expected_seq: i64,
        found_seq: Option<i64>,
        floor_seq: i64,
        head_seq: i64,
        epoch: i64,
    },

    /// A reconstructed row references content that is no longer retained.
    #[error("history reconstruction is missing chunk {digest} referenced by {referenced_by}")]
    HistoryMissingChunk {
        digest: String,
        referenced_by: String,
    },

    /// The reconstructed filesystem failed relational or visible-tree checks.
    #[error("history reconstruction failed integrity checks: {0}")]
    HistoryIntegrity(String),
}

/// Result type alias using the SDK Error type.
pub type Result<T> = std::result::Result<T, Error>;
