use std::collections::BTreeMap;

use turso::{Connection, Value};

use crate::config::Geometry;
use crate::error::{Error, Result};
use crate::filesystem::{FsError, Stats};

use super::{
    current_timestamp, write_commit_time_sets, PendingTimeChange, STORAGE_CHUNKED, STORAGE_INLINE,
};

pub(super) struct FileStorage {
    pub(super) size: u64,
    storage_kind: i64,
    inline_data: Option<Vec<u8>>,
}

pub(super) struct WriteRangeRef<'a> {
    pub(super) offset: u64,
    pub(super) data: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NormalizedWriteRange {
    pub(super) offset: u64,
    pub(super) data: Vec<u8>,
}

impl NormalizedWriteRange {
    pub(super) fn end(&self) -> u64 {
        self.offset + self.data.len() as u64
    }
}

pub(super) fn normalize_write_ranges(
    ranges: &[WriteRangeRef<'_>],
) -> Result<Vec<NormalizedWriteRange>> {
    let mut merged_ranges: BTreeMap<u64, Vec<u8>> = BTreeMap::new();

    for range in ranges {
        if range.data.is_empty() {
            continue;
        }

        let data_len = u64::try_from(range.data.len())
            .map_err(|_| Error::Internal("file write length overflow".to_string()))?;
        let write_start = range.offset;
        let write_end = write_start
            .checked_add(data_len)
            .ok_or_else(|| Error::Internal("file write offset overflow".to_string()))?;
        let mut start = write_start;
        let mut end = write_end;
        let mut existing_ranges = Vec::new();

        if let Some((&prev_start, prev_data)) = merged_ranges.range(..=write_start).next_back() {
            let prev_end = prev_start
                .checked_add(prev_data.len() as u64)
                .ok_or_else(|| Error::Internal("file write offset overflow".to_string()))?;

            if prev_end >= write_start {
                let prev_data = prev_data.clone();
                merged_ranges.remove(&prev_start);

                start = prev_start;
                end = end.max(prev_end);
                existing_ranges.push((prev_start, prev_data));
            }
        }

        loop {
            let next = merged_ranges
                .range(start..)
                .next()
                .map(|(&next_start, next_data)| (next_start, next_data.clone()));

            let Some((next_start, next_data)) = next else {
                break;
            };

            if next_start > end {
                break;
            }

            let next_end = next_start
                .checked_add(next_data.len() as u64)
                .ok_or_else(|| Error::Internal("file write offset overflow".to_string()))?;
            merged_ranges.remove(&next_start);

            end = end.max(next_end);
            existing_ranges.push((next_start, next_data));
        }

        let merged_len = usize::try_from(end - start)
            .map_err(|_| Error::Internal("file write range too large".to_string()))?;
        let mut merged = vec![0; merged_len];
        for (range_start, range_data) in existing_ranges {
            let range_offset = usize::try_from(range_start - start)
                .map_err(|_| Error::Internal("file write range too large".to_string()))?;
            merged[range_offset..range_offset + range_data.len()].copy_from_slice(&range_data);
        }

        let write_offset = usize::try_from(write_start - start)
            .map_err(|_| Error::Internal("file write range too large".to_string()))?;
        merged[write_offset..write_offset + range.data.len()].copy_from_slice(range.data);

        merged_ranges.insert(start, merged);
    }

    Ok(merged_ranges
        .into_iter()
        .map(|(offset, data)| NormalizedWriteRange { offset, data })
        .collect())
}

pub(super) fn dense_after_inline_write_batch(
    current_size: u64,
    new_size: u64,
    ranges: &[NormalizedWriteRange],
) -> bool {
    let mut covered_end = current_size;

    for range in ranges {
        let range_end = range.end();
        if range_end <= covered_end {
            continue;
        }
        if range.offset > covered_end {
            return false;
        }
        covered_end = range_end;
        if covered_end >= new_size {
            return true;
        }
    }

    covered_end >= new_size
}

pub(super) async fn file_storage(conn: &Connection, ino: i64) -> Result<FileStorage> {
    let mut stmt = conn
        .prepare_cached("SELECT size, storage_kind, data_inline FROM fs_inode WHERE ino = ?")
        .await?;
    let mut rows = stmt.query((ino,)).await?;

    if let Some(row) = rows.next().await? {
        let size = required_u64(&row, 0, "size")?;
        let storage_kind = required_i64(&row, 1, "storage_kind")?;
        if storage_kind != STORAGE_CHUNKED && storage_kind != STORAGE_INLINE {
            return Err(corrupt_column("storage_kind", "unknown storage kind"));
        }
        let inline_data = match row.get_value(2) {
            Ok(Value::Blob(data)) => Some(data),
            Ok(Value::Null) => None,
            Ok(_) | Err(_) => return Err(corrupt_column("data_inline", "expected blob or null")),
        };
        if storage_kind == STORAGE_INLINE && inline_data.is_none() {
            return Err(corrupt_column(
                "data_inline",
                "inline file missing inline data",
            ));
        }
        Ok(FileStorage {
            size,
            storage_kind,
            inline_data,
        })
    } else {
        Err(FsError::NotFound.into())
    }
}

// Architecture §2.1 exposes `read` as the free-function store seam. The
// AgentFSFile hot path uses `read_from_storage` after it has already fetched
// metadata, avoiding a duplicate metadata query.
#[allow(dead_code)]
pub(super) async fn read(
    conn: &Connection,
    ino: i64,
    geometry: Geometry,
    offset: u64,
    size: u64,
) -> Result<Vec<u8>> {
    let metadata = file_storage(conn, ino).await?;
    read_from_storage(conn, ino, geometry, &metadata, offset, size).await
}

pub(super) async fn read_from_storage(
    conn: &Connection,
    ino: i64,
    geometry: Geometry,
    metadata: &FileStorage,
    offset: u64,
    size: u64,
) -> Result<Vec<u8>> {
    if offset >= metadata.size || size == 0 {
        return Ok(Vec::new());
    }

    let size = std::cmp::min(size, metadata.size - offset);
    if metadata.storage_kind == STORAGE_INLINE {
        let mut result = Vec::with_capacity(size as usize);
        let inline_data = metadata.inline_data.clone().unwrap_or_default();
        let start = offset as usize;
        let requested = size as usize;

        if start < inline_data.len() {
            let available = std::cmp::min(inline_data.len() - start, requested);
            result.extend_from_slice(&inline_data[start..start + available]);
        }

        if result.len() < requested {
            result.resize(requested, 0);
        }

        return Ok(result);
    }

    read_chunked(conn, ino, geometry, offset, size).await
}

async fn read_chunked(
    conn: &Connection,
    ino: i64,
    geometry: Geometry,
    offset: u64,
    size: u64,
) -> Result<Vec<u8>> {
    let chunk_size = geometry.chunk_size as u64;
    let start_chunk = offset / chunk_size;
    let end_chunk = (offset + size).saturating_sub(1) / chunk_size;

    let mut stmt = conn
        .prepare_cached("SELECT chunk_index, data FROM fs_data WHERE ino = ? AND chunk_index >= ? AND chunk_index <= ? ORDER BY chunk_index")
        .await?;
    crate::profiling::record_chunk_read_query();
    let mut rows = stmt
        .query((ino, start_chunk as i64, end_chunk as i64))
        .await?;

    let mut result = Vec::with_capacity(size as usize);
    let start_offset_in_chunk = (offset % chunk_size) as usize;
    let mut next_expected_chunk = start_chunk;
    let mut chunks_read = 0u64;

    while let Some(row) = rows.next().await? {
        chunks_read += 1;
        let chunk_index = required_u64(&row, 0, "fs_data.chunk_index")?;

        while next_expected_chunk < chunk_index && result.len() < size as usize {
            let skip = if next_expected_chunk == start_chunk {
                start_offset_in_chunk
            } else {
                0
            };
            let zeros_needed =
                std::cmp::min(chunk_size as usize - skip, size as usize - result.len());
            result.extend(std::iter::repeat_n(0u8, zeros_needed));
            next_expected_chunk += 1;
        }

        let chunk_data = match row.get_value(1) {
            Ok(Value::Blob(data)) => data,
            Ok(_) | Err(_) => return Err(corrupt_column("fs_data.data", "expected blob")),
        };
        let skip = if chunk_index == start_chunk {
            start_offset_in_chunk
        } else {
            0
        };
        if skip >= chunk_data.len() {
            let zeros_needed =
                std::cmp::min(chunk_size as usize - skip, size as usize - result.len());
            result.extend(std::iter::repeat_n(0u8, zeros_needed));
        } else {
            let remaining = size as usize - result.len();
            let take = std::cmp::min(chunk_data.len() - skip, remaining);
            result.extend_from_slice(&chunk_data[skip..skip + take]);

            let chunk_end = skip + take;
            if chunk_end < chunk_size as usize && result.len() < size as usize {
                let zeros_needed = std::cmp::min(
                    chunk_size as usize - chunk_end,
                    size as usize - result.len(),
                );
                result.extend(std::iter::repeat_n(0u8, zeros_needed));
            }
        }
        next_expected_chunk = chunk_index + 1;
    }

    if result.len() < size as usize {
        result.resize(size as usize, 0);
    }

    crate::profiling::record_chunk_read_chunks(chunks_read);
    Ok(result)
}

/// `preserve_times`: when true (deferred batcher commits racing an explicit
/// chmod/chown/utimens), leave mtime/ctime untouched instead of stamping
/// the commit time. `explicit_times`: stashed setattr values folded into the
/// inode UPDATE itself (see `write_commit_time_sets`).
pub(super) async fn write_ranges(
    conn: &Connection,
    ino: i64,
    geometry: Geometry,
    ranges: &[WriteRangeRef<'_>],
    preserve_times: bool,
    explicit_times: Option<&PendingTimeChange>,
) -> Result<()> {
    let ranges = normalize_write_ranges(ranges)?;
    if ranges.is_empty() {
        return Ok(());
    }

    let metadata = file_storage(conn, ino).await?;
    let write_end = ranges
        .iter()
        .map(NormalizedWriteRange::end)
        .max()
        .unwrap_or(metadata.size);
    let new_size = std::cmp::max(metadata.size, write_end);

    if metadata.storage_kind == STORAGE_INLINE
        && new_size <= geometry.inline_threshold as u64
        && dense_after_inline_write_batch(metadata.size, new_size, &ranges)
    {
        let mut inline_data = metadata.inline_data.unwrap_or_default();
        inline_data.resize(metadata.size as usize, 0);
        inline_data.resize(new_size as usize, 0);
        for range in &ranges {
            let start = range.offset as usize;
            inline_data[start..start + range.data.len()].copy_from_slice(&range.data);
        }

        conn.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))
            .await?;
        let mut sets = vec!["size = ?", "data_inline = ?", "storage_kind = ?"];
        let mut values: Vec<Value> = vec![
            Value::Integer(new_size as i64),
            Value::Blob(inline_data),
            Value::Integer(STORAGE_INLINE),
        ];
        let (time_sets, time_values) = write_commit_time_sets(preserve_times, explicit_times)?;
        sets.extend(time_sets);
        values.extend(time_values);
        values.push(Value::Integer(ino));
        let sql = format!("UPDATE fs_inode SET {} WHERE ino = ?", sets.join(", "));
        conn.execute(&sql, values).await?;
        return Ok(());
    }

    let mut chunked_ranges = Vec::new();
    if metadata.storage_kind == STORAGE_INLINE {
        let mut inline_data = metadata.inline_data.unwrap_or_default();
        inline_data.resize(metadata.size as usize, 0);
        conn.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))
            .await?;
        if !inline_data.is_empty() {
            chunked_ranges.push(NormalizedWriteRange {
                offset: 0,
                data: inline_data,
            });
        }
    } else {
        conn.execute(
            "UPDATE fs_inode SET data_inline = NULL, storage_kind = ? WHERE ino = ?",
            (STORAGE_CHUNKED, ino),
        )
        .await?;
    }

    chunked_ranges.extend(ranges);
    write_ranges_chunked(conn, ino, geometry, &chunked_ranges).await?;

    let mut sets = vec!["size = ?", "data_inline = NULL", "storage_kind = ?"];
    let mut values: Vec<Value> = vec![
        Value::Integer(new_size as i64),
        Value::Integer(STORAGE_CHUNKED),
    ];
    let (time_sets, time_values) = write_commit_time_sets(preserve_times, explicit_times)?;
    sets.extend(time_sets);
    values.extend(time_values);
    values.push(Value::Integer(ino));
    let sql = format!("UPDATE fs_inode SET {} WHERE ino = ?", sets.join(", "));
    conn.execute(&sql, values).await?;

    Ok(())
}

pub(super) async fn truncate(
    conn: &Connection,
    ino: i64,
    geometry: Geometry,
    new_size: u64,
) -> Result<()> {
    let metadata = file_storage(conn, ino).await?;

    if metadata.storage_kind == STORAGE_INLINE {
        if new_size <= geometry.inline_threshold as u64 {
            let mut inline_data = metadata.inline_data.unwrap_or_default();
            inline_data.resize(metadata.size as usize, 0);
            inline_data.resize(new_size as usize, 0);
            conn.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))
                .await?;
            let (now_secs, now_nsec) = current_timestamp()?;
            conn.execute(
                "UPDATE fs_inode SET size = ?, data_inline = ?, storage_kind = ?, mtime = ?, ctime = ?, mtime_nsec = ?, ctime_nsec = ? WHERE ino = ?",
                (
                    new_size as i64,
                    Value::Blob(inline_data),
                    STORAGE_INLINE,
                    now_secs,
                    now_secs,
                    now_nsec,
                    now_nsec,
                    ino,
                ),
            )
            .await?;
            return Ok(());
        }

        let mut inline_data = metadata.inline_data.unwrap_or_default();
        inline_data.resize(metadata.size as usize, 0);
        transition_inline_to_chunked(conn, ino, geometry, &inline_data).await?;
        truncate_chunked_data(conn, ino, geometry, metadata.size, new_size).await?;
        update_chunked_truncate_metadata(conn, ino, new_size).await?;
        return Ok(());
    }

    if new_size <= geometry.inline_threshold as u64 {
        if let Some(inline_data) =
            read_dense_prefix_for_inline(conn, ino, geometry, new_size).await?
        {
            conn.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))
                .await?;
            let (now_secs, now_nsec) = current_timestamp()?;
            conn.execute(
                "UPDATE fs_inode SET size = ?, data_inline = ?, storage_kind = ?, mtime = ?, ctime = ?, mtime_nsec = ?, ctime_nsec = ? WHERE ino = ?",
                (
                    new_size as i64,
                    Value::Blob(inline_data),
                    STORAGE_INLINE,
                    now_secs,
                    now_secs,
                    now_nsec,
                    now_nsec,
                    ino,
                ),
            )
            .await?;
            return Ok(());
        }
    }

    truncate_chunked_data(conn, ino, geometry, metadata.size, new_size).await?;
    update_chunked_truncate_metadata(conn, ino, new_size).await?;
    Ok(())
}

async fn transition_inline_to_chunked(
    conn: &Connection,
    ino: i64,
    geometry: Geometry,
    inline_data: &[u8],
) -> Result<()> {
    conn.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))
        .await?;

    if !inline_data.is_empty() {
        write_data_at_offset(conn, ino, geometry, 0, inline_data).await?;
    }

    conn.execute(
        "UPDATE fs_inode SET data_inline = NULL, storage_kind = ? WHERE ino = ?",
        (STORAGE_CHUNKED, ino),
    )
    .await?;

    Ok(())
}

async fn read_dense_prefix_for_inline(
    conn: &Connection,
    ino: i64,
    geometry: Geometry,
    new_size: u64,
) -> Result<Option<Vec<u8>>> {
    if new_size == 0 {
        return Ok(Some(Vec::new()));
    }

    let chunk_size = geometry.chunk_size as u64;
    let last_chunk = (new_size - 1) / chunk_size;
    let mut inline_data = Vec::with_capacity(new_size as usize);

    let mut stmt = conn
        .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
        .await?;
    for chunk_idx in 0..=last_chunk {
        stmt.reset()?;
        let mut rows = stmt.query((ino, chunk_idx as i64)).await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let chunk_data = match row.get_value(0) {
            Ok(Value::Blob(data)) => data,
            _ => return Ok(None),
        };
        let remaining = new_size as usize - inline_data.len();
        let needed = std::cmp::min(geometry.chunk_size, remaining);
        if chunk_data.len() < needed {
            return Ok(None);
        }
        inline_data.extend_from_slice(&chunk_data[..needed]);
    }

    Ok(Some(inline_data))
}

async fn truncate_chunked_data(
    conn: &Connection,
    ino: i64,
    geometry: Geometry,
    current_size: u64,
    new_size: u64,
) -> Result<()> {
    let chunk_size = geometry.chunk_size as u64;

    if new_size == 0 {
        conn.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))
            .await?;
    } else if new_size < current_size {
        let last_chunk_idx = (new_size - 1) / chunk_size;

        conn.execute(
            "DELETE FROM fs_data WHERE ino = ? AND chunk_index > ?",
            (ino, last_chunk_idx as i64),
        )
        .await?;

        let end_in_last_chunk = ((new_size - 1) % chunk_size + 1) as usize;
        if end_in_last_chunk < chunk_size as usize {
            let mut stmt = conn
                .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
                .await?;
            let mut rows = stmt.query((ino, last_chunk_idx as i64)).await?;

            if let Some(row) = rows.next().await? {
                if let Ok(Value::Blob(chunk_data)) = row.get_value(0) {
                    if chunk_data.len() > end_in_last_chunk {
                        conn.execute(
                            "UPDATE fs_data SET data = ? WHERE ino = ? AND chunk_index = ?",
                            (&chunk_data[..end_in_last_chunk], ino, last_chunk_idx as i64),
                        )
                        .await?;
                    }
                }
            }
        }
    } else if new_size > current_size {
        let last_existing_chunk = if current_size == 0 {
            None
        } else {
            Some((current_size - 1) / chunk_size)
        };
        let last_new_chunk = (new_size - 1) / chunk_size;

        if let Some(last_idx) = last_existing_chunk {
            let mut stmt = conn
                .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
                .await?;
            let mut rows = stmt.query((ino, last_idx as i64)).await?;

            if let Some(row) = rows.next().await? {
                if let Ok(Value::Blob(chunk_data)) = row.get_value(0) {
                    let current_chunk_len = chunk_data.len();
                    let needed_len = if last_idx == last_new_chunk {
                        ((new_size - 1) % chunk_size + 1) as usize
                    } else {
                        chunk_size as usize
                    };

                    if needed_len > current_chunk_len {
                        let mut padded = chunk_data.clone();
                        padded.resize(needed_len, 0);
                        conn.execute(
                            "UPDATE fs_data SET data = ? WHERE ino = ? AND chunk_index = ?",
                            (&padded[..], ino, last_idx as i64),
                        )
                        .await?;
                    }
                }
            }
        }

        let start_new_chunk = last_existing_chunk.map(|i| i + 1).unwrap_or(0);
        for chunk_idx in start_new_chunk..=last_new_chunk {
            let chunk_len = if chunk_idx == last_new_chunk {
                ((new_size - 1) % chunk_size + 1) as usize
            } else {
                chunk_size as usize
            };
            let zeros = vec![0u8; chunk_len];
            conn.execute(
                "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)",
                (ino, chunk_idx as i64, &zeros[..]),
            )
            .await?;
        }
    }

    Ok(())
}

async fn update_chunked_truncate_metadata(
    conn: &Connection,
    ino: i64,
    new_size: u64,
) -> Result<()> {
    let (now_secs, now_nsec) = current_timestamp()?;
    conn.execute(
        "UPDATE fs_inode SET size = ?, data_inline = NULL, storage_kind = ?, mtime = ?, ctime = ?, mtime_nsec = ?, ctime_nsec = ? WHERE ino = ?",
        (
            new_size as i64,
            STORAGE_CHUNKED,
            now_secs,
            now_secs,
            now_nsec,
            now_nsec,
            ino,
        ),
    )
    .await?;
    Ok(())
}

async fn write_data_at_offset(
    conn: &Connection,
    ino: i64,
    geometry: Geometry,
    offset: u64,
    data: &[u8],
) -> Result<()> {
    let ranges = [WriteRangeRef { offset, data }];
    let ranges = normalize_write_ranges(&ranges)?;
    write_ranges_chunked(conn, ino, geometry, &ranges).await
}

async fn write_ranges_chunked(
    conn: &Connection,
    ino: i64,
    geometry: Geometry,
    ranges: &[NormalizedWriteRange],
) -> Result<()> {
    let chunk_size = geometry.chunk_size as u64;

    if ranges.is_empty() {
        return Ok(());
    }

    let mut select_stmt = conn
        .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
        .await?;
    let mut insert_stmt = conn
        .prepare_cached("INSERT OR REPLACE INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)")
        .await?;

    let mut chunks: BTreeMap<i64, Vec<u8>> = BTreeMap::new();

    for range in ranges {
        let mut written = 0usize;
        while written < range.data.len() {
            let current_offset = range.offset + written as u64;
            let chunk_index = (current_offset / chunk_size) as i64;
            let offset_in_chunk = (current_offset % chunk_size) as usize;

            let remaining_in_chunk = geometry.chunk_size - offset_in_chunk;
            let remaining_data = range.data.len() - written;
            let to_write = std::cmp::min(remaining_in_chunk, remaining_data);
            let write_slice = &range.data[written..written + to_write];

            if offset_in_chunk == 0 && to_write == geometry.chunk_size {
                chunks.insert(chunk_index, write_slice.to_vec());
                written += to_write;
                continue;
            }

            if let std::collections::btree_map::Entry::Vacant(entry) = chunks.entry(chunk_index) {
                let mut rows = select_stmt.query((ino, chunk_index)).await?;
                let chunk_data = if let Some(row) = rows.next().await? {
                    match row.get_value(0) {
                        Ok(Value::Blob(data)) => data,
                        Ok(_) | Err(_) => {
                            return Err(corrupt_column("fs_data.data", "expected blob"))
                        }
                    }
                } else {
                    Vec::new()
                };
                select_stmt.reset()?;
                entry.insert(chunk_data);
            }

            let chunk_data = chunks
                .get_mut(&chunk_index)
                .expect("chunk must be loaded before partial write");
            if chunk_data.len() < offset_in_chunk + to_write {
                chunk_data.resize(offset_in_chunk + to_write, 0);
            }
            chunk_data[offset_in_chunk..offset_in_chunk + to_write].copy_from_slice(write_slice);

            written += to_write;
        }
    }

    let chunks_written = chunks.len() as u64;
    // Tier Three Axis H investigation: tried a multi-row VALUES batch
    // with up to 32 rows per execute() but measured slower wall-time in
    // 5-iter runs, suggesting libSQL doesn't share the
    // prepared-statement cost reduction across different VALUES
    // arities or that the per-execute setup cost dwarfed any saved
    // round-trips on our workload sizes. Reverted to the cached
    // single-row prepared statement.
    for (chunk_index, chunk_data) in chunks {
        insert_stmt
            .execute((ino, chunk_index, Value::Blob(chunk_data)))
            .await?;
        insert_stmt.reset()?;
    }

    crate::profiling::record_chunk_write_chunks(chunks_written);
    Ok(())
}

pub(super) async fn link_count(conn: &Connection, ino: i64) -> Result<u32> {
    let mut stmt = conn
        .prepare_cached("SELECT nlink FROM fs_inode WHERE ino = ?")
        .await?;
    let mut rows = stmt.query((ino,)).await?;

    if let Some(row) = rows.next().await? {
        required_u32(&row, 0, "nlink")
    } else {
        Ok(0)
    }
}

pub(super) async fn mode(conn: &Connection, ino: i64) -> Result<Option<u32>> {
    let mut stmt = conn
        .prepare_cached("SELECT mode FROM fs_inode WHERE ino = ?")
        .await?;
    let mut rows = stmt.query((ino,)).await?;

    if let Some(row) = rows.next().await? {
        Ok(Some(required_u32(&row, 0, "mode")?))
    } else {
        Ok(None)
    }
}

pub(super) async fn getattr(conn: &Connection, ino: i64) -> Result<Option<Stats>> {
    let mut stmt = conn
        .prepare_cached("SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec FROM fs_inode WHERE ino = ?")
        .await?;
    let mut rows = stmt.query((ino,)).await?;

    if let Some(row) = rows.next().await? {
        Ok(Some(stats_from_row(&row)?))
    } else {
        Ok(None)
    }
}

pub(super) fn stats_from_row(row: &turso::Row) -> Result<Stats> {
    stats_from_row_at(row, 0)
}

pub(super) fn stats_from_row_at(row: &turso::Row, start: usize) -> Result<Stats> {
    let size = required_i64(row, start + 5, "size")?;
    if size < 0 {
        return Err(corrupt_column("size", "negative file size"));
    }
    Ok(Stats {
        ino: required_i64(row, start, "ino")?,
        mode: required_u32(row, start + 1, "mode")?,
        nlink: required_u32(row, start + 2, "nlink")?,
        uid: required_u32(row, start + 3, "uid")?,
        gid: required_u32(row, start + 4, "gid")?,
        size,
        atime: required_i64(row, start + 6, "atime")?,
        mtime: required_i64(row, start + 7, "mtime")?,
        ctime: required_i64(row, start + 8, "ctime")?,
        atime_nsec: required_u32(row, start + 10, "atime_nsec")?,
        mtime_nsec: required_u32(row, start + 11, "mtime_nsec")?,
        ctime_nsec: required_u32(row, start + 12, "ctime_nsec")?,
        rdev: required_u64(row, start + 9, "rdev")?,
    })
}

fn required_i64(row: &turso::Row, index: usize, column: &str) -> Result<i64> {
    row.get_value(index)
        .ok()
        .and_then(|value| value.as_integer().copied())
        .ok_or_else(|| corrupt_column(column, "expected integer"))
}

fn required_u32(row: &turso::Row, index: usize, column: &str) -> Result<u32> {
    let value = required_i64(row, index, column)?;
    u32::try_from(value).map_err(|_| corrupt_column(column, "integer out of u32 range"))
}

fn required_u64(row: &turso::Row, index: usize, column: &str) -> Result<u64> {
    let value = required_i64(row, index, column)?;
    u64::try_from(value).map_err(|_| corrupt_column(column, "negative integer"))
}

fn corrupt_column(column: &str, reason: &str) -> Error {
    Error::Fs(FsError::Corrupt(format!("invalid {column}: {reason}")))
}
