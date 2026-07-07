use agentfs_core::fs::WriteRange;
use agentfs_core::{error::Error as SdkError, BoxedFile};
use std::collections::BTreeMap;
use tokio::runtime::Runtime;

/// Threshold at which the FUSE-layer per-fh write coalescer flushes its
/// accumulated ranges down to the SDK. Picked at 4x the chunk size so a single
/// flushed call covers a few SQLite chunks and the AsyncMutex acquisition in
/// the SDK write batcher is amortised across many FUSE_WRITE requests for the
/// same handle. Smaller writes (the common git-clone case) accumulate in this
/// buffer until `flush` / `release` arrives and only then hit the SDK.
pub(super) const FUSE_COALESCE_FLUSH_BYTES: usize = 256 * 1024;

/// Tracks an open FUSE file handle and its pending coalesced writes.
pub(super) struct OpenFile {
    /// Inode associated with this FUSE file handle.
    pub(super) ino: u64,
    /// The file handle from the filesystem layer.
    pub(super) file: BoxedFile,
    /// Pending writes buffered for coalescing before reaching the filesystem layer.
    pub(super) pending: WriteBuffer,
}

impl OpenFile {
    pub(super) fn new(ino: u64, file: BoxedFile) -> Self {
        Self {
            ino,
            file,
            pending: WriteBuffer::default(),
        }
    }

    #[cfg(test)]
    pub(super) fn buffer_write(&mut self, offset: u64, data: &[u8]) -> Result<(), i32> {
        self.pending.write(offset, data)?;
        Ok(())
    }

    /// Coalesce a single FUSE write into the per-fh pending buffer. Returns
    /// `true` if the cumulative buffer size has reached the flush threshold
    /// and the caller should drain it before replying to the kernel.
    pub(super) fn buffer_fuse_write(&mut self, offset: u64, data: &[u8]) -> Result<bool, i32> {
        self.pending.write(offset, data)?;
        Ok(self.pending.bytes >= FUSE_COALESCE_FLUSH_BYTES)
    }

    /// Drain the per-fh pending buffer into a `(file, ranges, range_count,
    /// byte_count)` tuple so the caller can release the surrounding
    /// `open_files` lock before issuing the async `pwrite_ranges*` call. The
    /// hot write path MUST NOT hold the parking_lot `open_files` mutex across
    /// `runtime.block_on(...)`: doing so serializes every other FUSE handler
    /// behind one fh's SQLite commit and was the source of a 2x checkout
    /// regression observed in the first Tier Two benchmark pass.
    pub(super) fn take_pending(&mut self) -> Option<PendingDrain> {
        if self.pending.is_empty() {
            return None;
        }
        let file = self.file.clone();
        let ranges = self.pending.ranges_for_flush();
        let range_count = ranges.len() as u64;
        let byte_count = ranges
            .iter()
            .map(|range| range.data.len() as u64)
            .sum::<u64>();
        self.pending.clear();
        Some((file, ranges, range_count, byte_count))
    }

    /// Synchronous flush via the non-batched pwrite API. Production code uses
    /// `take_pending` + `flush_pending_batched_out_of_lock` instead; this
    /// remains as a test-only convenience so the OpenFile unit tests stay
    /// readable.
    #[cfg(test)]
    pub(super) fn flush_pending(&mut self, runtime: &Runtime) -> Result<(), SdkError> {
        let Some((file, ranges, range_count, byte_count)) = self.take_pending() else {
            return Ok(());
        };

        runtime.block_on(async move { file.pwrite_ranges(ranges).await })?;
        crate::telemetry::record_fuse_flush(range_count, byte_count);
        Ok(())
    }
}

/// `(file, ranges, range_count, byte_count)` drained from a pending
/// `WriteBuffer`; flushed out-of-lock via `flush_pending_batched_out_of_lock`.
pub(super) type PendingDrain = (BoxedFile, Vec<WriteRange>, u64, u64);

/// Flush a `(file, ranges, range_count, byte_count)` tuple produced by
/// `OpenFile::take_pending()` via the SDK write batcher (so the coalesced
/// ranges enter the cross-inode batched-commit path). Called by the FUSE
/// write / flush / release handlers AFTER they have released the
/// `open_files` parking_lot mutex.
pub(super) fn flush_pending_batched_out_of_lock(
    runtime: &Runtime,
    drain: PendingDrain,
) -> Result<(), SdkError> {
    let (file, ranges, range_count, byte_count) = drain;
    runtime.block_on(async move { file.pwrite_ranges_batched(ranges).await })?;
    crate::telemetry::record_fuse_flush(range_count, byte_count);
    Ok(())
}

/// Pending write ranges for one open FUSE file handle.
///
/// Ranges are keyed by start offset and kept non-overlapping. Adjacent and
/// overlapping writes are merged eagerly so common sequential writes become one
/// filesystem-layer `pwrite` when the handle is flushed.
#[derive(Default)]
pub(super) struct WriteBuffer {
    pub(super) ranges: BTreeMap<u64, Vec<u8>>,
    pub(super) bytes: usize,
}

impl WriteBuffer {
    pub(super) fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(super) fn clear(&mut self) {
        self.ranges.clear();
        self.bytes = 0;
    }

    pub(super) fn ranges_for_flush(&self) -> Vec<WriteRange> {
        self.ranges
            .iter()
            .map(|(&offset, data)| WriteRange {
                offset,
                data: data.clone(),
            })
            .collect()
    }

    pub(super) fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), i32> {
        if data.is_empty() {
            return Ok(());
        }

        let data_len = u64::try_from(data.len()).map_err(|_| libc::EINVAL)?;
        let write_start = offset;
        let write_end = offset.checked_add(data_len).ok_or(libc::EINVAL)?;
        let mut start = write_start;
        let mut end = write_end;
        let mut existing_ranges = Vec::new();

        if let Some((&prev_start, prev_data)) = self.ranges.range(..=write_start).next_back() {
            let prev_end = prev_start
                .checked_add(prev_data.len() as u64)
                .ok_or(libc::EINVAL)?;

            if prev_end >= write_start {
                let prev_data = prev_data.clone();
                self.ranges.remove(&prev_start);
                self.bytes -= prev_data.len();

                start = prev_start;
                end = end.max(prev_end);
                existing_ranges.push((prev_start, prev_data));
            }
        }

        loop {
            let next = self
                .ranges
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
                .ok_or(libc::EINVAL)?;
            self.ranges.remove(&next_start);
            self.bytes -= next_data.len();

            end = end.max(next_end);
            existing_ranges.push((next_start, next_data));
        }

        let mut merged = vec![0; (end - start) as usize];
        for (range_start, range_data) in existing_ranges {
            let range_offset = (range_start - start) as usize;
            merged[range_offset..range_offset + range_data.len()].copy_from_slice(&range_data);
        }

        let write_offset = (write_start - start) as usize;
        merged[write_offset..write_offset + data.len()].copy_from_slice(data);

        self.bytes += merged.len();
        self.ranges.insert(start, merged);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenFile, WriteBuffer};
    use agentfs_core::fs::{Stats, WriteRange, S_IFREG};
    use agentfs_core::{BoxedFile, File};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::runtime::Runtime;

    fn ranges(buffer: &WriteBuffer) -> Vec<(u64, Vec<u8>)> {
        buffer
            .ranges_for_flush()
            .into_iter()
            .map(|range| (range.offset, range.data))
            .collect()
    }

    #[derive(Default)]
    struct RecordingFile {
        pwrite_calls: AtomicUsize,
        pwrite_ranges_calls: AtomicUsize,
        ranges: Mutex<Vec<WriteRange>>,
    }

    #[async_trait::async_trait]
    impl File for RecordingFile {
        async fn pread(&self, _offset: u64, _size: u64) -> agentfs_core::error::Result<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn pwrite(&self, _offset: u64, _data: &[u8]) -> agentfs_core::error::Result<()> {
            self.pwrite_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn pwrite_ranges(&self, ranges: Vec<WriteRange>) -> agentfs_core::error::Result<()> {
            self.pwrite_ranges_calls.fetch_add(1, Ordering::SeqCst);
            *self.ranges.lock().unwrap() = ranges;
            Ok(())
        }

        async fn truncate(&self, _size: u64) -> agentfs_core::error::Result<()> {
            Ok(())
        }

        async fn fsync(&self) -> agentfs_core::error::Result<()> {
            Ok(())
        }

        async fn fstat(&self) -> agentfs_core::error::Result<Stats> {
            Ok(stats(1, S_IFREG | 0o644))
        }
    }

    fn stats(ino: i64, mode: u32) -> Stats {
        Stats {
            ino,
            mode,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            size: 123,
            atime: 1,
            mtime: 2,
            ctime: 3,
            atime_nsec: 4,
            mtime_nsec: 5,
            ctime_nsec: 6,
            rdev: 0,
        }
    }

    #[test]
    fn write_buffer_merges_adjacent_ranges() {
        let mut buffer = WriteBuffer::default();

        buffer.write(0, b"hello").unwrap();
        buffer.write(5, b" world").unwrap();

        assert_eq!(buffer.bytes(), 11);
        assert_eq!(ranges(&buffer), vec![(0, b"hello world".to_vec())]);
    }

    #[test]
    fn write_buffer_overlays_overlapping_writes() {
        let mut buffer = WriteBuffer::default();

        buffer.write(0, b"abcdef").unwrap();
        buffer.write(2, b"ZZ").unwrap();

        assert_eq!(buffer.bytes(), 6);
        assert_eq!(ranges(&buffer), vec![(0, b"abZZef".to_vec())]);
    }

    #[test]
    fn write_buffer_overlays_following_range() {
        let mut buffer = WriteBuffer::default();

        buffer.write(10, b"abc").unwrap();
        buffer.write(8, b"ZZZZ").unwrap();

        assert_eq!(buffer.bytes(), 5);
        assert_eq!(ranges(&buffer), vec![(8, b"ZZZZc".to_vec())]);
    }

    #[test]
    fn write_buffer_bridges_two_existing_ranges() {
        let mut buffer = WriteBuffer::default();

        buffer.write(0, b"ab").unwrap();
        buffer.write(4, b"ef").unwrap();
        buffer.write(2, b"cd").unwrap();

        assert_eq!(buffer.bytes(), 6);
        assert_eq!(ranges(&buffer), vec![(0, b"abcdef".to_vec())]);
    }

    #[test]
    fn write_buffer_keeps_disjoint_ranges_ordered() {
        let mut buffer = WriteBuffer::default();

        buffer.write(10, b"tail").unwrap();
        buffer.write(0, b"head").unwrap();

        assert_eq!(buffer.bytes(), 8);
        assert_eq!(
            ranges(&buffer),
            vec![(0, b"head".to_vec()), (10, b"tail".to_vec())]
        );
    }

    #[test]
    fn write_buffer_rejects_offset_overflow() {
        let mut buffer = WriteBuffer::default();

        assert_eq!(buffer.write(u64::MAX, b"x"), Err(libc::EINVAL));
        assert!(buffer.is_empty());
    }

    #[test]
    fn open_file_flushes_pending_writes_with_batch_api() {
        let runtime = Runtime::new().unwrap();
        let recorder = Arc::new(RecordingFile::default());
        let file: BoxedFile = recorder.clone();
        let mut open_file = OpenFile::new(1, file);

        open_file.buffer_write(0, b"head").unwrap();
        open_file.buffer_write(10, b"tail").unwrap();
        open_file.flush_pending(&runtime).unwrap();

        assert_eq!(recorder.pwrite_calls.load(Ordering::SeqCst), 0);
        assert_eq!(recorder.pwrite_ranges_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *recorder.ranges.lock().unwrap(),
            vec![
                WriteRange {
                    offset: 0,
                    data: b"head".to_vec(),
                },
                WriteRange {
                    offset: 10,
                    data: b"tail".to_vec(),
                },
            ]
        );
        assert!(open_file.pending.is_empty());
    }
}
