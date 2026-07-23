//! Durability acknowledgements shared by transport adapters.
//!
//! A handler that acknowledges a write must say whether the acknowledgement is
//! volatile or committed. The facade then owns the mechanical contract for
//! turning that declaration into filesystem work.

use super::handles::{Authority, Handle, HandleTable};
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
    pub(crate) count: usize,
    pub durability: AckDurability,
}

/// Shared semantics facade for adapter-visible filesystem contracts.
#[derive(Clone)]
pub struct Semantics {
    fs: Arc<dyn FileSystem>,
    handles: HandleTable,
}

impl Semantics {
    /// Create a semantics facade over the canonical filesystem implementation.
    pub fn new(fs: Arc<dyn FileSystem>) -> Self {
        Self::new_with_handle_table(fs, HandleTable::default())
    }

    fn new_with_handle_table(fs: Arc<dyn FileSystem>, handles: HandleTable) -> Self {
        fs.register_reap_hook(Arc::new(handles.clone()));
        Self { fs, handles }
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
    pub(crate) async fn write(
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

    /// Write bytes through a semantics-owned cached handle.
    pub async fn write_handle(
        &self,
        handle: &Handle,
        offset: u64,
        data: &[u8],
        durability: AckDurability,
    ) -> Result<WriteReceipt> {
        self.write(handle.file(), offset, data, durability).await
    }

    /// Return a cached open handle for `ino` with the requested authority.
    pub async fn open_cached(&self, ino: i64, authority: Authority) -> Result<Handle> {
        self.handles.open_cached(&self.fs, ino, authority).await
    }

    /// Insert a caller-generated write-authority token.
    pub fn try_grant_write_authority_with_token(&self, ino: i64, token: u64) -> bool {
        self.handles
            .try_grant_write_authority_with_token(ino, token)
    }

    /// Check and refresh write authority carried by a token.
    pub fn has_write_authority(&self, ino: i64, token: u64) -> bool {
        self.handles.has_write_authority(ino, token)
    }

    /// Return a live token for READDIRPLUS, if this inode has write authority.
    pub fn authority_token_for_ino(&self, ino: i64) -> Option<u64> {
        self.handles.authority_token_for_ino(ino)
    }

    /// Invalidate all cached handles and write-authority tokens for an inode.
    pub fn invalidate_handles(&self, ino: i64) {
        self.handles.invalidate_ino(ino);
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
