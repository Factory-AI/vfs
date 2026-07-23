//! Filesystem operation reply
//!
//! A reply is passed to filesystem operation implementations and must be used to send back the
//! result of an operation. The reply can optionally be sent to another thread to asynchronously
//! work on an operation and provide the result later. Also it allows replying with a block of
//! data without cloning the data. A reply *must always* be used (by calling either `ok()` or
//! `error()` exactly once).

use super::ll::{
    self,
    reply::{DirEntPlusList, DirEntryPlus},
    Generation,
};
use super::ll::{
    reply::{DirEntList, DirEntOffset, DirEntry},
    INodeNo,
};
use libc::c_int;
use std::convert::AsRef;
use std::ffi::OsStr;
use std::fmt;
use std::io::IoSlice;
use std::time::Duration;
use tracing::{error, warn};

use super::{FileAttr, FileType};

/// Generic reply callback to send data
pub(crate) trait ReplySender: Send + Sync + Unpin + 'static {
    /// Send data.
    fn send(&self, data: &[IoSlice<'_>]) -> std::io::Result<()>;
}

impl fmt::Debug for Box<dyn ReplySender> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(f, "Box<ReplySender>")
    }
}

/// Generic reply trait
pub(crate) trait Reply {
    /// Create a new reply for the given request
    fn new<S: ReplySender>(unique: u64, sender: S) -> Self;
}

///
/// Raw reply
///
#[derive(Debug)]
pub(crate) struct ReplyRaw {
    /// Unique id of the request to reply to
    unique: ll::RequestId,
    /// Closure to call for sending the reply
    sender: Option<Box<dyn ReplySender>>,
}

impl Reply for ReplyRaw {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyRaw {
        let sender = Box::new(sender);
        ReplyRaw {
            unique: ll::RequestId(unique),
            sender: Some(sender),
        }
    }
}

impl ReplyRaw {
    /// Reply to a request with the given error code and data. Must be called
    /// only once (the `ok` and `error` methods ensure this by consuming `self`)
    fn send_ll_mut(&mut self, response: &ll::Response<'_>) {
        assert!(self.sender.is_some());
        let sender = self.sender.take().unwrap();
        let res = response.with_iovec(self.unique, |iov| sender.send(iov));
        if let Err(err) = res {
            error!("Failed to send FUSE reply: {err}");
        }
    }
    fn send_ll(mut self, response: &ll::Response<'_>) {
        self.send_ll_mut(response);
    }

    /// Reply to a request with the given error code
    fn error(self, err: c_int) {
        assert_ne!(err, 0);
        self.send_ll(&ll::Response::new_error(ll::Errno::from_i32(err)));
    }
}

impl Drop for ReplyRaw {
    fn drop(&mut self) {
        if self.sender.is_some() {
            warn!(
                "Reply not sent for operation {}, replying with I/O error",
                self.unique.0
            );
            self.send_ll_mut(&ll::Response::new_error(ll::Errno::EIO));
        }
    }
}

///
/// Empty reply
///
#[derive(Debug)]
pub(crate) struct ReplyEmpty {
    reply: ReplyRaw,
}

impl Reply for ReplyEmpty {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyEmpty {
        ReplyEmpty {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyEmpty {
    /// Reply to a request with nothing
    pub(crate) fn ok(self) {
        self.reply.send_ll(&ll::Response::new_empty());
    }

    /// Reply to a request with the given error code
    #[allow(
        dead_code,
        reason = "AgentFS statfs path currently replies only success"
    )]
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Data reply
///
#[derive(Debug)]
pub(crate) struct ReplyData {
    reply: ReplyRaw,
}

impl Reply for ReplyData {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyData {
        ReplyData {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyData {
    /// Reply to a request with the given data
    pub(crate) fn data(self, data: &[u8]) {
        self.reply.send_ll(&ll::Response::new_slice(data));
    }

    /// Reply to a request with the given error code
    #[allow(
        dead_code,
        reason = "AgentFS statfs path currently replies only success"
    )]
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Entry reply
///
#[derive(Debug)]
pub(crate) struct ReplyEntry {
    reply: ReplyRaw,
}

impl Reply for ReplyEntry {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyEntry {
        ReplyEntry {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyEntry {
    /// Reply to a request with the given entry
    #[allow(dead_code, reason = "adapter uses separate TTL entry replies")]
    pub(crate) fn entry(self, ttl: &Duration, attr: &FileAttr, generation: u64) {
        self.entry_with_ttls(ttl, ttl, attr, generation);
    }

    /// Reply to a request with the given entry and separate entry/attribute TTLs.
    pub(crate) fn entry_with_ttls(
        self,
        entry_ttl: &Duration,
        attr_ttl: &Duration,
        attr: &FileAttr,
        generation: u64,
    ) {
        self.reply.send_ll(&ll::Response::new_entry(
            ll::INodeNo(attr.ino),
            ll::Generation(generation),
            &attr.into(),
            *attr_ttl,
            *entry_ttl,
        ));
    }

    /// Reply to a lookup with a cacheable negative entry.
    pub(crate) fn negative(self, ttl: &Duration) {
        self.reply.send_ll(&ll::Response::new_negative_entry(*ttl));
    }

    /// Reply to a request with the given error code
    #[allow(
        dead_code,
        reason = "AgentFS statfs path currently replies only success"
    )]
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Attribute Reply
///
#[derive(Debug)]
pub(crate) struct ReplyAttr {
    reply: ReplyRaw,
}

impl Reply for ReplyAttr {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyAttr {
        ReplyAttr {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyAttr {
    /// Reply to a request with the given attribute
    pub(crate) fn attr(self, ttl: &Duration, attr: &FileAttr) {
        self.reply
            .send_ll(&ll::Response::new_attr(ttl, &attr.into()));
    }

    /// Reply to a request with the given error code
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Open Reply
///
#[derive(Debug)]
pub(crate) struct ReplyOpen {
    reply: ReplyRaw,
}

impl Reply for ReplyOpen {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyOpen {
        ReplyOpen {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyOpen {
    /// Reply to a request with the given open result
    pub(crate) fn opened(self, fh: u64, flags: u32) {
        self.reply
            .send_ll(&ll::Response::new_open(ll::FileHandle(fh), flags));
    }

    /// Reply to a request with the given error code
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Write Reply
///
#[derive(Debug)]
pub(crate) struct ReplyWrite {
    reply: ReplyRaw,
}

impl Reply for ReplyWrite {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyWrite {
        ReplyWrite {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyWrite {
    /// Reply to a request with the number of bytes written
    pub(crate) fn written(self, size: u32) {
        self.reply.send_ll(&ll::Response::new_write(size));
    }

    /// Reply to a request with the given error code
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Statfs Reply
///
#[derive(Debug)]
pub(crate) struct ReplyStatfs {
    reply: ReplyRaw,
}

impl Reply for ReplyStatfs {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyStatfs {
        ReplyStatfs {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyStatfs {
    /// Reply to a statfs request with filesystem information
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn statfs(
        self,
        blocks: u64,
        bfree: u64,
        bavail: u64,
        files: u64,
        ffree: u64,
        bsize: u32,
        namelen: u32,
        frsize: u32,
    ) {
        self.reply.send_ll(&ll::Response::new_statfs(
            blocks, bfree, bavail, files, ffree, bsize, namelen, frsize,
        ));
    }

    /// Reply to a request with the given error code
    #[allow(
        dead_code,
        reason = "AgentFS statfs path currently replies only success"
    )]
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Create reply
///
#[derive(Debug)]
pub(crate) struct ReplyCreate {
    reply: ReplyRaw,
}

impl Reply for ReplyCreate {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyCreate {
        ReplyCreate {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyCreate {
    /// Reply to a request with a newly created file entry and its newly open file handle
    /// # Panics
    /// When attempting to use kernel passthrough. Use `opened_passthrough()` instead.
    #[allow(dead_code, reason = "adapter uses separate TTL create replies")]
    pub(crate) fn created(
        self,
        ttl: &Duration,
        attr: &FileAttr,
        generation: u64,
        fh: u64,
        flags: u32,
    ) {
        self.created_with_ttls(ttl, ttl, attr, generation, fh, flags);
    }

    /// Reply to a request with a newly created file entry and separate entry/attribute TTLs.
    /// # Panics
    /// When attempting to use kernel passthrough. Use `opened_passthrough()` instead.
    pub(crate) fn created_with_ttls(
        self,
        entry_ttl: &Duration,
        attr_ttl: &Duration,
        attr: &FileAttr,
        generation: u64,
        fh: u64,
        flags: u32,
    ) {
        self.reply.send_ll(&ll::Response::new_create(
            attr_ttl,
            entry_ttl,
            &attr.into(),
            ll::Generation(generation),
            ll::FileHandle(fh),
            flags,
        ));
    }

    /// Reply to a request with the given error code
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Lock Reply
///
#[derive(Debug)]
pub(crate) struct ReplyLock {
    reply: ReplyRaw,
}

impl Reply for ReplyLock {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyLock {
        ReplyLock {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyLock {
    /// Reply to a request with a file lock
    #[allow(
        dead_code,
        reason = "success reply retained for unsupported lock operation"
    )]
    pub(crate) fn locked(self, start: u64, end: u64, typ: i32, pid: u32) {
        self.reply.send_ll(&ll::Response::new_lock(&ll::Lock {
            range: (start, end),
            typ,
            pid,
        }));
    }

    /// Reply to a request with the given error code
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Bmap Reply
///
#[derive(Debug)]
pub(crate) struct ReplyBmap {
    reply: ReplyRaw,
}

impl Reply for ReplyBmap {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyBmap {
        ReplyBmap {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyBmap {
    /// Reply to a request with a bmap
    #[allow(
        dead_code,
        reason = "success reply retained for unsupported bmap operation"
    )]
    pub(crate) fn bmap(self, block: u64) {
        self.reply.send_ll(&ll::Response::new_bmap(block));
    }

    /// Reply to a request with the given error code
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Ioctl Reply
///
#[derive(Debug)]
pub(crate) struct ReplyIoctl {
    reply: ReplyRaw,
}

impl Reply for ReplyIoctl {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyIoctl {
        ReplyIoctl {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyIoctl {
    /// Reply to a request with an ioctl
    #[allow(
        dead_code,
        reason = "success reply retained for unsupported ioctl operation"
    )]
    pub(crate) fn ioctl(self, result: i32, data: &[u8]) {
        self.reply
            .send_ll(&ll::Response::new_ioctl(result, &[IoSlice::new(data)]));
    }

    /// Reply to a request with the given error code
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Poll Reply
///
#[derive(Debug)]
pub(crate) struct ReplyPoll {
    reply: ReplyRaw,
}

impl Reply for ReplyPoll {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyPoll {
        ReplyPoll {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyPoll {
    /// Reply to a request with ready poll events
    #[allow(
        dead_code,
        reason = "success reply retained for unsupported poll operation"
    )]
    pub(crate) fn poll(self, revents: u32) {
        self.reply.send_ll(&ll::Response::new_poll(revents));
    }

    /// Reply to a request with the given error code
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Directory reply
///
#[derive(Debug)]
pub(crate) struct ReplyDirectory {
    reply: ReplyRaw,
    data: DirEntList,
}

impl ReplyDirectory {
    /// Creates a new `ReplyDirectory` with a specified buffer size.
    pub(crate) fn new<S: ReplySender>(unique: u64, sender: S, size: usize) -> ReplyDirectory {
        ReplyDirectory {
            reply: Reply::new(unique, sender),
            data: DirEntList::new(size),
        }
    }

    /// Add an entry to the directory reply buffer. Returns true if the buffer is full.
    /// A transparent offset value can be provided for each entry. The kernel uses these
    /// value to request the next entries in further readdir calls
    #[must_use]
    #[allow(
        dead_code,
        reason = "READDIRPLUS path is the active directory reply path"
    )]
    pub(crate) fn add<T: AsRef<OsStr>>(
        &mut self,
        ino: u64,
        offset: i64,
        kind: FileType,
        name: T,
    ) -> bool {
        let name = name.as_ref();
        self.data.push(&DirEntry::new(
            INodeNo(ino),
            DirEntOffset(offset),
            kind,
            name,
        ))
    }

    /// Reply to a request with the filled directory buffer
    pub(crate) fn ok(self) {
        self.reply.send_ll(&self.data.into());
    }

    /// Reply to a request with the given error code
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// `DirectoryPlus` reply
///
#[derive(Debug)]
pub(crate) struct ReplyDirectoryPlus {
    reply: ReplyRaw,
    buf: DirEntPlusList,
}

impl ReplyDirectoryPlus {
    /// Creates a new `ReplyDirectory` with a specified buffer size.
    pub(crate) fn new<S: ReplySender>(unique: u64, sender: S, size: usize) -> ReplyDirectoryPlus {
        ReplyDirectoryPlus {
            reply: Reply::new(unique, sender),
            buf: DirEntPlusList::new(size),
        }
    }

    /// Add an entry to the directory reply buffer. Returns true if the buffer is full.
    /// A transparent offset value can be provided for each entry. The kernel uses these
    /// value to request the next entries in further readdir calls
    #[allow(dead_code, reason = "adapter uses separate TTL readdirplus entries")]
    pub(crate) fn add<T: AsRef<OsStr>>(
        &mut self,
        ino: u64,
        offset: i64,
        name: T,
        ttl: &Duration,
        attr: &FileAttr,
        generation: u64,
    ) -> bool {
        self.add_with_ttls(ino, offset, name, ttl, ttl, attr, generation)
    }

    /// Add an entry to the directory-plus reply buffer with separate entry/attribute TTLs.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_with_ttls<T: AsRef<OsStr>>(
        &mut self,
        ino: u64,
        offset: i64,
        name: T,
        entry_ttl: &Duration,
        attr_ttl: &Duration,
        attr: &FileAttr,
        generation: u64,
    ) -> bool {
        let name = name.as_ref();
        self.buf.push(&DirEntryPlus::new(
            INodeNo(ino),
            Generation(generation),
            DirEntOffset(offset),
            name,
            *entry_ttl,
            attr.into(),
            *attr_ttl,
        ))
    }

    /// Reply to a request with the filled directory buffer
    pub(crate) fn ok(self) {
        self.reply.send_ll(&self.buf.into());
    }

    /// Reply to a request with the given error code
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Xattr reply
///
#[derive(Debug)]
pub(crate) struct ReplyXattr {
    reply: ReplyRaw,
}

impl Reply for ReplyXattr {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyXattr {
        ReplyXattr {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyXattr {
    /// Reply to a request with the size of an extended attribute
    #[allow(
        dead_code,
        reason = "success reply retained for unsupported xattr operation"
    )]
    pub(crate) fn size(self, size: u32) {
        self.reply.send_ll(&ll::Response::new_xattr_size(size));
    }

    /// Reply to a request with the data of an extended attribute
    #[allow(
        dead_code,
        reason = "success reply retained for unsupported xattr operation"
    )]
    pub(crate) fn data(self, data: &[u8]) {
        self.reply.send_ll(&ll::Response::new_slice(data));
    }

    /// Reply to a request with the given error code.
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

///
/// Lseek Reply
///
#[derive(Debug)]
pub(crate) struct ReplyLseek {
    reply: ReplyRaw,
}

impl Reply for ReplyLseek {
    fn new<S: ReplySender>(unique: u64, sender: S) -> ReplyLseek {
        ReplyLseek {
            reply: Reply::new(unique, sender),
        }
    }
}

impl ReplyLseek {
    /// Reply to a request with seeked offset
    #[allow(
        dead_code,
        reason = "success reply retained for unsupported lseek operation"
    )]
    pub(crate) fn offset(self, offset: i64) {
        self.reply.send_ll(&ll::Response::new_lseek(offset));
    }

    /// Reply to a request with the given error code
    pub(crate) fn error(self, err: c_int) {
        self.reply.error(err);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::IoSlice;
    use std::sync::mpsc::{sync_channel, SyncSender};
    use std::thread;
    use std::time::{Duration, UNIX_EPOCH};
    use zerocopy::{Immutable, IntoBytes};

    #[derive(Debug, IntoBytes, Immutable)]
    #[repr(C)]
    struct Data {
        a: u8,
        b: u8,
        c: u16,
    }

    #[test]
    fn serialize_empty() {
        assert!(().as_bytes().is_empty());
    }

    #[test]
    fn serialize_slice() {
        let data: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
        assert_eq!(data.as_bytes(), [0x12, 0x34, 0x56, 0x78]);
    }

    #[test]
    fn serialize_struct() {
        let data = Data {
            a: 0x12,
            b: 0x34,
            c: 0x5678,
        };
        assert_eq!(data.as_bytes(), [0x12, 0x34, 0x78, 0x56]);
    }

    struct AssertSender {
        expected: Vec<u8>,
    }

    impl super::ReplySender for AssertSender {
        fn send(&self, data: &[IoSlice<'_>]) -> std::io::Result<()> {
            let mut v = vec![];
            for x in data {
                v.extend_from_slice(x);
            }
            assert_eq!(self.expected, v);
            Ok(())
        }
    }

    #[test]
    fn reply_raw() {
        let data = Data {
            a: 0x12,
            b: 0x34,
            c: 0x5678,
        };
        let sender = AssertSender {
            expected: vec![
                0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
                0x00, 0x00, 0x12, 0x34, 0x78, 0x56,
            ],
        };
        let reply: ReplyRaw = Reply::new(0xdeadbeef, sender);
        reply.send_ll(&ll::Response::new_data(data.as_bytes()));
    }

    #[test]
    fn reply_error() {
        let sender = AssertSender {
            expected: vec![
                0x10, 0x00, 0x00, 0x00, 0xbe, 0xff, 0xff, 0xff, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
                0x00, 0x00,
            ],
        };
        let reply: ReplyRaw = Reply::new(0xdeadbeef, sender);
        reply.error(66);
    }

    #[test]
    fn reply_empty() {
        let sender = AssertSender {
            expected: vec![
                0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
                0x00, 0x00,
            ],
        };
        let reply: ReplyEmpty = Reply::new(0xdeadbeef, sender);
        reply.ok();
    }

    #[test]
    fn reply_data() {
        let sender = AssertSender {
            expected: vec![
                0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
                0x00, 0x00, 0xde, 0xad, 0xbe, 0xef,
            ],
        };
        let reply: ReplyData = Reply::new(0xdeadbeef, sender);
        reply.data(&[0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn reply_entry() {
        let mut expected = vec![
            0x88, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
            0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xaa, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x65, 0x87, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x65, 0x87,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x43, 0x00, 0x00, 0x21, 0x43, 0x00, 0x00,
            0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x22, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x33, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x34, 0x12, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x34, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x34, 0x12,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x56, 0x00, 0x00, 0x78, 0x56, 0x00, 0x00,
            0x78, 0x56, 0x00, 0x00, 0xa4, 0x81, 0x00, 0x00, 0x55, 0x00, 0x00, 0x00, 0x66, 0x00,
            0x00, 0x00, 0x77, 0x00, 0x00, 0x00, 0x88, 0x00, 0x00, 0x00,
        ];

        expected.extend(vec![0xbb, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        expected[0] = (expected.len()) as u8;

        let sender = AssertSender { expected };
        let reply: ReplyEntry = Reply::new(0xdeadbeef, sender);
        let time = UNIX_EPOCH + Duration::new(0x1234, 0x5678);
        let ttl = Duration::new(0x8765, 0x4321);
        let attr = FileAttr {
            ino: 0x11,
            size: 0x22,
            blocks: 0x33,
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 0x55,
            uid: 0x66,
            gid: 0x77,
            rdev: 0x88,
            flags: 0x99,
            blksize: 0xbb,
        };
        reply.entry(&ttl, &attr, 0xaa);
    }

    #[test]
    fn reply_attr() {
        let mut expected = vec![
            0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
            0x00, 0x00, 0x65, 0x87, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x43, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x22, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x33, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x34, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x34, 0x12, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x34, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x56, 0x00, 0x00,
            0x78, 0x56, 0x00, 0x00, 0x78, 0x56, 0x00, 0x00, 0xa4, 0x81, 0x00, 0x00, 0x55, 0x00,
            0x00, 0x00, 0x66, 0x00, 0x00, 0x00, 0x77, 0x00, 0x00, 0x00, 0x88, 0x00, 0x00, 0x00,
        ];

        expected.extend_from_slice(&[0xbb, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        expected[0] = expected.len() as u8;

        let sender = AssertSender { expected };
        let reply: ReplyAttr = Reply::new(0xdeadbeef, sender);
        let time = UNIX_EPOCH + Duration::new(0x1234, 0x5678);
        let ttl = Duration::new(0x8765, 0x4321);
        let attr = FileAttr {
            ino: 0x11,
            size: 0x22,
            blocks: 0x33,
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 0x55,
            uid: 0x66,
            gid: 0x77,
            rdev: 0x88,
            flags: 0x99,
            blksize: 0xbb,
        };
        reply.attr(&ttl, &attr);
    }

    #[test]
    fn reply_open() {
        let sender = AssertSender {
            expected: vec![
                0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
                0x00, 0x00, 0x22, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x33, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00,
            ],
        };
        let reply: ReplyOpen = Reply::new(0xdeadbeef, sender);
        reply.opened(0x1122, 0x33);
    }

    #[test]
    fn reply_write() {
        let sender = AssertSender {
            expected: vec![
                0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
                0x00, 0x00, 0x22, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
        };
        let reply: ReplyWrite = Reply::new(0xdeadbeef, sender);
        reply.written(0x1122);
    }

    #[test]
    fn reply_statfs() {
        let sender = AssertSender {
            expected: vec![
                0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
                0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x22, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x33, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x44, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x55, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x66, 0x00, 0x00, 0x00, 0x77, 0x00, 0x00, 0x00, 0x88, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
        };
        let reply: ReplyStatfs = Reply::new(0xdeadbeef, sender);
        reply.statfs(0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88);
    }

    #[test]
    fn reply_create() {
        let mut expected = vec![
            0x98, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
            0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xaa, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x65, 0x87, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x65, 0x87,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x43, 0x00, 0x00, 0x21, 0x43, 0x00, 0x00,
            0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x22, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x33, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x34, 0x12, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x34, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x34, 0x12,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x56, 0x00, 0x00, 0x78, 0x56, 0x00, 0x00,
            0x78, 0x56, 0x00, 0x00, 0xa4, 0x81, 0x00, 0x00, 0x55, 0x00, 0x00, 0x00, 0x66, 0x00,
            0x00, 0x00, 0x77, 0x00, 0x00, 0x00, 0x88, 0x00, 0x00, 0x00, 0xbb, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0xcc, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let insert_at = expected.len() - 16;
        expected.splice(
            insert_at..insert_at,
            vec![0xdd, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        );
        expected[0] = (expected.len()) as u8;

        let sender = AssertSender { expected };
        let reply: ReplyCreate = Reply::new(0xdeadbeef, sender);
        let time = UNIX_EPOCH + Duration::new(0x1234, 0x5678);
        let ttl = Duration::new(0x8765, 0x4321);
        let attr = FileAttr {
            ino: 0x11,
            size: 0x22,
            blocks: 0x33,
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 0x55,
            uid: 0x66,
            gid: 0x77,
            rdev: 0x88,
            flags: 0x99,
            blksize: 0xdd,
        };
        reply.created(&ttl, &attr, 0xaa, 0xbb, 0xcc);
    }

    #[test]
    fn reply_lock() {
        let sender = AssertSender {
            expected: vec![
                0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
                0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x22, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x33, 0x00, 0x00, 0x00, 0x44, 0x00, 0x00, 0x00,
            ],
        };
        let reply: ReplyLock = Reply::new(0xdeadbeef, sender);
        reply.locked(0x11, 0x22, 0x33, 0x44);
    }

    #[test]
    fn reply_bmap() {
        let sender = AssertSender {
            expected: vec![
                0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
                0x00, 0x00, 0x34, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
        };
        let reply: ReplyBmap = Reply::new(0xdeadbeef, sender);
        reply.bmap(0x1234);
    }

    #[test]
    fn reply_directory() {
        let sender = AssertSender {
            expected: vec![
                0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
                0x00, 0x00, 0xbb, 0xaa, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x68, 0x65,
                0x6c, 0x6c, 0x6f, 0x00, 0x00, 0x00, 0xdd, 0xcc, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x00,
                0x00, 0x00, 0x77, 0x6f, 0x72, 0x6c, 0x64, 0x2e, 0x72, 0x73,
            ],
        };
        let mut reply = ReplyDirectory::new(0xdeadbeef, sender, 4096);
        assert!(!reply.add(0xaabb, 1, FileType::Directory, "hello"));
        assert!(!reply.add(0xccdd, 2, FileType::RegularFile, "world.rs"));
        reply.ok();
    }

    #[test]
    fn reply_xattr_size() {
        let sender = AssertSender {
            expected: vec![
                0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xEF, 0xBE, 0xAD, 0xDE, 0x00, 0x00,
                0x00, 0x00, 0x78, 0x56, 0x34, 0x12, 0x00, 0x00, 0x00, 0x00,
            ],
        };
        let reply = ReplyXattr::new(0xdeadbeef, sender);
        reply.size(0x12345678);
    }

    #[test]
    fn reply_xattr_data() {
        let sender = AssertSender {
            expected: vec![
                0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xEF, 0xBE, 0xAD, 0xDE, 0x00, 0x00,
                0x00, 0x00, 0x11, 0x22, 0x33, 0x44,
            ],
        };
        let reply = ReplyXattr::new(0xdeadbeef, sender);
        reply.data(&[0x11, 0x22, 0x33, 0x44]);
    }

    impl super::ReplySender for SyncSender<()> {
        fn send(&self, _: &[IoSlice<'_>]) -> std::io::Result<()> {
            self.send(()).unwrap();
            Ok(())
        }
    }

    #[test]
    fn async_reply() {
        let (tx, rx) = sync_channel::<()>(1);
        let reply: ReplyEmpty = Reply::new(0xdeadbeef, tx);
        thread::spawn(move || {
            reply.ok();
        });
        rx.recv().unwrap();
    }
}
