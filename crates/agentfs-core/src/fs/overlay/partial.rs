use super::super::agentfs::store::{self, ChunkWriteHooks, WriteRangeRef};
use super::*;
use crate::fs::{File, FsError, WriteRange};
use std::sync::Arc;
use turso::transaction::{Transaction, TransactionBehavior};

#[derive(Debug, Clone)]
pub(super) struct PartialOrigin {
    pub(super) base_path: String,
    pub(super) base_fingerprint_size: i64,
    pub(super) base_mtime: i64,
    pub(super) base_mtime_nsec: u32,
    pub(super) base_ctime: i64,
    pub(super) base_ctime_nsec: u32,
}

pub(super) struct OverlayPartialFile {
    pub(super) delta: AgentFS,
    pub(super) base: Arc<dyn FileSystem>,
    pub(super) base_file: super::super::BoxedFile,
    pub(super) origin: PartialOrigin,
    pub(super) overlay_ino: i64,
    pub(super) delta_ino: i64,
    pub(super) chunk_size: usize,
}

struct PartialOriginChunkHooks<'a> {
    file: &'a OverlayPartialFile,
}

#[async_trait]
impl ChunkWriteHooks for PartialOriginChunkHooks<'_> {
    async fn seed_missing_chunk(
        &self,
        conn: &Connection,
        ino: i64,
        geometry: crate::config::Geometry,
        chunk_index: i64,
    ) -> Result<Option<Vec<u8>>> {
        debug_assert_eq!(ino, self.file.delta_ino);
        let chunk_index = u64::try_from(chunk_index)
            .map_err(|_| Error::Internal("negative chunk index".to_string()))?;
        let base_size = self.file.partial_base_size_with_conn(conn).await?;
        let chunk_start = chunk_index
            .checked_mul(geometry.chunk_size as u64)
            .ok_or_else(|| Error::Internal("chunk offset overflow".to_string()))?;
        if chunk_start >= base_size {
            return Ok(None);
        }

        self.file.validate_current_origin().await?;
        let readable = std::cmp::min(geometry.chunk_size as u64, base_size - chunk_start);
        let mut chunk = self.file.base_file.pread(chunk_start, readable).await?;
        chunk.resize(geometry.chunk_size, 0);
        Ok(Some(chunk))
    }

    async fn chunk_written(&self, conn: &Connection, ino: i64, chunk_index: i64) -> Result<()> {
        debug_assert_eq!(ino, self.file.delta_ino);
        conn.execute(
            "INSERT OR IGNORE INTO fs_chunk_override (delta_ino, chunk_index) VALUES (?, ?)",
            (ino, chunk_index),
        )
        .await?;
        Ok(())
    }
}

impl OverlayFS {
    pub(super) async fn partial_origin_for_delta(
        &self,
        delta_ino: i64,
    ) -> Result<Option<PartialOrigin>> {
        let conn = self.delta.get_connection().await?;
        let mut rows = conn
            .query(
                "SELECT base_path, base_size, base_fingerprint_size,
                        base_mtime, base_mtime_nsec, base_ctime, base_ctime_nsec
                 FROM fs_partial_origin WHERE delta_ino = ?",
                (delta_ino,),
            )
            .await?;
        if let Some(row) = rows.next().await? {
            let base_path = match row.get_value(0)? {
                Value::Text(path) => path,
                _ => {
                    return Err(Error::Internal(
                        "invalid partial origin base_path".to_string(),
                    ))
                }
            };
            let base_size = row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .ok_or_else(|| Error::Internal("invalid partial origin base_size".to_string()))?;
            let base_fingerprint_size = row
                .get_value(2)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(base_size);
            let base_mtime = row
                .get_value(3)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);
            let base_mtime_nsec = row
                .get_value(4)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;
            let base_ctime = row
                .get_value(5)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);
            let base_ctime_nsec = row
                .get_value(6)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;
            Ok(Some(PartialOrigin {
                base_path,
                base_fingerprint_size: if base_fingerprint_size < 0 {
                    base_size
                } else {
                    base_fingerprint_size
                },
                base_mtime,
                base_mtime_nsec,
                base_ctime,
                base_ctime_nsec,
            }))
        } else {
            Ok(None)
        }
    }

    pub(super) async fn add_partial_origin_mapping(
        &self,
        delta_ino: i64,
        base_ino: i64,
        base_path: &str,
        base_stats: &Stats,
    ) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        conn.execute(
            "INSERT OR REPLACE INTO fs_partial_origin (
                delta_ino, base_ino, base_path, base_size, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            (delta_ino, base_ino, base_path, base_stats.size, now),
        )
        .await?;
        conn.execute(
            "UPDATE fs_partial_origin
             SET base_fingerprint_size = ?1, base_mtime = ?2, base_mtime_nsec = ?3
             WHERE delta_ino = ?4",
            (
                base_stats.size,
                base_stats.mtime,
                base_stats.mtime_nsec as i64,
                delta_ino,
            ),
        )
        .await?;
        conn.execute(
            "UPDATE fs_partial_origin
             SET base_ctime = ?1, base_ctime_nsec = ?2
             WHERE delta_ino = ?3",
            (base_stats.ctime, base_stats.ctime_nsec as i64, delta_ino),
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl File for OverlayPartialFile {
    async fn pread(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        self.validate_current_origin().await?;
        let conn = self.delta.get_connection().await?;
        let file_size = self.delta_file_size_with_conn(&conn).await?;
        if offset >= file_size || size == 0 {
            return Ok(Vec::new());
        }

        let read_len = std::cmp::min(size, file_size - offset) as usize;
        let chunk_size = self.chunk_size as u64;
        let mut result = Vec::with_capacity(read_len);

        while result.len() < read_len {
            let current_offset = offset + result.len() as u64;
            let chunk_index = current_offset / chunk_size;
            let offset_in_chunk = (current_offset % chunk_size) as usize;
            let take = std::cmp::min(
                self.chunk_size - offset_in_chunk,
                read_len.saturating_sub(result.len()),
            );

            let chunk = self.read_merged_chunk_with_conn(&conn, chunk_index).await?;
            result.extend_from_slice(&chunk[offset_in_chunk..offset_in_chunk + take]);
        }

        Ok(result)
    }

    async fn pwrite(&self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.pwrite_ranges(vec![WriteRange {
            offset,
            data: data.to_vec(),
        }])
        .await
    }

    async fn pwrite_ranges(&self, ranges: Vec<WriteRange>) -> Result<()> {
        if ranges.iter().all(|range| range.data.is_empty()) {
            return Ok(());
        }
        let conn = self.delta.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let range_refs: Vec<_> = ranges
            .iter()
            .map(|range| WriteRangeRef {
                offset: range.offset,
                data: range.data.as_slice(),
            })
            .collect();
        let hooks = PartialOriginChunkHooks { file: self };

        let result: Result<()> = async {
            store::write_ranges_with_chunk_hooks(
                &conn,
                self.delta_ino,
                self.geometry(),
                &range_refs,
                &hooks,
            )
            .await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                self.delta.invalidate_attr(self.delta_ino);
                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    async fn truncate(&self, size: u64) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let hooks = PartialOriginChunkHooks { file: self };

        let result: Result<()> = async {
            store::truncate_with_chunk_hooks(&conn, self.delta_ino, self.geometry(), size, &hooks)
                .await?;
            self.prune_chunk_overrides_after_truncate(&conn, size)
                .await?;

            let origin_base_size = self.partial_base_size_with_conn(&conn).await?;
            if size < origin_base_size {
                conn.execute(
                    "UPDATE fs_partial_origin SET base_size = ? WHERE delta_ino = ?",
                    (size as i64, self.delta_ino),
                )
                .await?;
            }
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                self.delta.invalidate_attr(self.delta_ino);
                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    async fn fsync(&self) -> Result<()> {
        self.delta.fsync().await
    }

    async fn fstat(&self) -> Result<Stats> {
        let mut stats = FileSystem::getattr(&self.delta, self.delta_ino)
            .await?
            .ok_or(FsError::NotFound)?;
        stats.ino = self.overlay_ino;
        Ok(stats)
    }
}

impl OverlayPartialFile {
    fn geometry(&self) -> crate::config::Geometry {
        crate::config::Geometry {
            chunk_size: self.chunk_size,
            inline_threshold: self.delta.inline_threshold(),
        }
    }

    async fn resolve_origin_base_stats(&self) -> Result<Option<Stats>> {
        let mut ino = ROOT_INO;
        if self.origin.base_path == "/" {
            return self.base.getattr(ino).await;
        }

        let mut stats = None;
        for component in self.origin.base_path.split('/').filter(|s| !s.is_empty()) {
            let Some(next) = self.base.lookup(ino, component).await? else {
                return Ok(None);
            };
            ino = next.ino;
            stats = Some(next);
        }
        Ok(stats)
    }

    async fn validate_current_origin(&self) -> Result<()> {
        let stats = self
            .resolve_origin_base_stats()
            .await?
            .ok_or(FsError::NotFound)?;
        if stats.size != self.origin.base_fingerprint_size
            || stats.mtime != self.origin.base_mtime
            || stats.mtime_nsec != self.origin.base_mtime_nsec
            || stats.ctime != self.origin.base_ctime
            || stats.ctime_nsec != self.origin.base_ctime_nsec
        {
            return Err(Error::Internal(format!(
                "partial-origin base changed for {}",
                self.origin.base_path
            )));
        }
        Ok(())
    }

    async fn delta_file_size_with_conn(&self, conn: &Connection) -> Result<u64> {
        let mut rows = conn
            .query("SELECT size FROM fs_inode WHERE ino = ?", (self.delta_ino,))
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64)
        } else {
            Err(FsError::NotFound.into())
        }
    }

    async fn prune_chunk_overrides_after_truncate(
        &self,
        conn: &Connection,
        size: u64,
    ) -> Result<()> {
        if size == 0 {
            conn.execute(
                "DELETE FROM fs_chunk_override WHERE delta_ino = ?",
                (self.delta_ino,),
            )
            .await?;
            return Ok(());
        }

        let last_chunk = (size - 1) / self.chunk_size as u64;
        conn.execute(
            "DELETE FROM fs_chunk_override WHERE delta_ino = ? AND chunk_index > ?",
            (self.delta_ino, last_chunk as i64),
        )
        .await?;
        Ok(())
    }

    async fn partial_base_size_with_conn(&self, conn: &Connection) -> Result<u64> {
        let mut rows = conn
            .query(
                "SELECT base_size FROM fs_partial_origin WHERE delta_ino = ?",
                (self.delta_ino,),
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64)
        } else {
            Err(FsError::NotFound.into())
        }
    }

    async fn chunk_is_override_with_conn(
        &self,
        conn: &Connection,
        chunk_index: u64,
    ) -> Result<bool> {
        let mut rows = conn
            .query(
                "SELECT 1 FROM fs_chunk_override WHERE delta_ino = ? AND chunk_index = ?",
                (self.delta_ino, chunk_index as i64),
            )
            .await?;
        Ok(rows.next().await?.is_some())
    }

    async fn read_merged_chunk_with_conn(
        &self,
        conn: &Connection,
        chunk_index: u64,
    ) -> Result<Vec<u8>> {
        if self.chunk_is_override_with_conn(conn, chunk_index).await? {
            let chunk_start = chunk_index
                .checked_mul(self.chunk_size as u64)
                .ok_or_else(|| Error::Internal("chunk offset overflow".to_string()))?;
            let mut chunk = store::read(
                conn,
                self.delta_ino,
                self.geometry(),
                chunk_start,
                self.chunk_size as u64,
            )
            .await?;
            chunk.resize(self.chunk_size, 0);
            return Ok(chunk);
        }

        let base_size = self.partial_base_size_with_conn(conn).await?;
        let chunk_start = chunk_index
            .checked_mul(self.chunk_size as u64)
            .ok_or_else(|| Error::Internal("chunk offset overflow".to_string()))?;
        let mut chunk = if chunk_start < base_size {
            self.validate_current_origin().await?;
            let readable = std::cmp::min(self.chunk_size as u64, base_size - chunk_start);
            self.base_file.pread(chunk_start, readable).await?
        } else {
            Vec::new()
        };
        chunk.resize(self.chunk_size, 0);
        Ok(chunk)
    }
}
