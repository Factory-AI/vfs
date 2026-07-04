//! Durability acknowledgements shared by transport adapters.
//!
//! A handler that acknowledges a write must say whether the acknowledgement is
//! volatile or committed. The facade then owns the mechanical contract for
//! turning that declaration into filesystem work.

use crate::error::Result;
use crate::fs::{BoxedFile, FileSystem, Stats};
use std::sync::Arc;

/// Durability level attached to a write acknowledgement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AckDurability {
    /// The bytes may still be buffered by an in-memory batcher.
    Volatile,
    /// The bytes must be committed before the acknowledgement is returned.
    Committed,
}

/// Receipt returned by [`Semantics::write`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteReceipt {
    pub count: usize,
    pub durability: AckDurability,
}

/// Shared semantics facade for adapter-visible filesystem contracts.
#[derive(Clone)]
pub struct Semantics {
    fs: Arc<dyn FileSystem>,
}

impl Semantics {
    /// Create a semantics facade over the canonical filesystem implementation.
    pub fn new(fs: Arc<dyn FileSystem>) -> Self {
        Self { fs }
    }

    /// Return coherent attributes for `ino`.
    ///
    /// Filesystem implementations own their pending-write visibility primitive.
    /// AgentFS merges batcher pending state into attributes before returning;
    /// adapters should use this method for NFS GETATTR and WCC attributes
    /// instead of bypassing the shared coherence contract.
    pub async fn stat_coherent(&self, ino: i64) -> Result<Option<Stats>> {
        self.fs.getattr(ino).await
    }

    /// Write bytes through an already-open file handle with an explicit ack
    /// durability declaration.
    pub async fn write(
        &self,
        file: &BoxedFile,
        offset: u64,
        data: &[u8],
        durability: AckDurability,
    ) -> Result<WriteReceipt> {
        file.pwrite(offset, data).await?;
        if matches!(durability, AckDurability::Committed) {
            file.fsync().await?;
        }
        Ok(WriteReceipt {
            count: data.len(),
            durability,
        })
    }

    /// Commit pending writes for one inode or for the whole filesystem.
    ///
    /// `Some(ino)` is the inode-level barrier used by FUSE fsync and NFS COMMIT
    /// once that protocol path lands. `None` is the filesystem-wide drain and
    /// checkpoint barrier used before clean shutdown/finalize.
    pub async fn commit_barrier(&self, ino: Option<i64>) -> Result<()> {
        match ino {
            Some(ino) => self.fs.drain_inode_writes(ino).await,
            None => self.fs.drain_all().await,
        }
    }

    /// Finalize a clean shutdown after the caller has crossed any required
    /// commit barrier.
    pub async fn finalize(&self) -> Result<()> {
        self.fs.finalize().await
    }
}
