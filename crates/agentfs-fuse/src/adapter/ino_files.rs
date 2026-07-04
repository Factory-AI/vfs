use super::write_buffer::{PendingDrain, WriteBuffer};
use agentfs_core::BoxedFile;

/// Per-inode file state for the zero-message-open path. With the kernel's
/// `no_open` latched (ENOSYS-OPEN), file ops arrive with `fh=0` and no
/// per-handle state exists: all I/O for an inode shares one resolved
/// `BoxedFile` and one coalescing write buffer. Entries live until FORGET
/// drops the inode (or soft-cap eviction reclaims a clean entry).
pub(super) struct InoFile {
    pub(super) file: BoxedFile,
    /// Pending writes buffered for coalescing, shared by every open(2) of
    /// this inode. Cross-handle write ordering is inherent (one buffer).
    pub(super) pending: WriteBuffer,
    /// False while the file was resolved for reads only. The first write op
    /// re-resolves with `O_RDWR` (triggering overlay copy-up) and replaces
    /// `file`, so post-copy-up reads go through the delta layer.
    pub(super) write_capable: bool,
    /// Resolution stamp consulted by the soft-cap eviction scan.
    pub(super) last_used: u64,
}

impl InoFile {
    /// See `OpenFile::take_pending` for the out-of-lock drain contract.
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
}
