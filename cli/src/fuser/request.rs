//! Filesystem operation request
//!
//! A request represents information about a filesystem operation the kernel driver wants us to
//! perform.  Unlike classic fuser, this `Request` owns its backing byte buffer so it can be
//! moved across thread boundaries (needed for the parallel worker pool introduced in Phase 8).
//! The buffer is held as an aligned owned allocation and the `ll::AnyRequest` view is parsed
//! on demand from it.

use super::ll::{fuse_abi as abi, Errno, Response};
use log::{debug, error, warn};
use std::convert::TryFrom;
use std::convert::TryInto;
use std::path::Path;
use std::sync::Arc;

use super::channel::ChannelSender;
use super::deferred_notify::DeferredNotifier;
use super::ll::Request as _;
use super::notify::Notifier;
use super::reply::ReplyDirectoryPlus;
use super::reply::{Reply, ReplyDirectory, ReplySender};
use super::session::{SessionACL, SessionShared};
use super::Filesystem;
use super::PollHandle;
use super::{ll, KernelConfig};

/// Classify a parsed request into a per-op latency slot (see
/// `agentfs_sdk::profiling::FuseOpSlot`).
fn fuse_op_slot(op: &ll::Operation<'_>) -> agentfs_sdk::profiling::FuseOpSlot {
    use agentfs_sdk::profiling::FuseOpSlot as Slot;
    match op {
        ll::Operation::Lookup(_) => Slot::Lookup,
        ll::Operation::GetAttr(_) => Slot::GetAttr,
        ll::Operation::SetAttr(_) => Slot::SetAttr,
        ll::Operation::Open(_) => Slot::Open,
        ll::Operation::Create(_) => Slot::Create,
        ll::Operation::Read(_) => Slot::Read,
        ll::Operation::Write(_) => Slot::Write,
        ll::Operation::Flush(_) => Slot::Flush,
        ll::Operation::Release(_) => Slot::Release,
        ll::Operation::ReadDirPlus(_) => Slot::ReadDirPlus,
        ll::Operation::Forget(_) | ll::Operation::BatchForget(_) => Slot::Forget,
        _ => Slot::Other,
    }
}

/// Owned, aligned buffer suitable for holding a FUSE request payload coming off /dev/fuse.
///
/// The `fuse_in_header` struct requires 4-byte alignment; we conservatively align to 8 bytes
/// which is sufficient for every FUSE ABI struct we dereference through zerocopy.
pub(crate) struct AlignedRequestBuf {
    storage: Box<[u64]>,
    len: usize,
}

impl AlignedRequestBuf {
    /// Allocate a new buffer sized to hold at least `capacity` bytes (rounded up to a `u64`).
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let word_capacity = capacity.div_ceil(std::mem::size_of::<u64>()).max(1);
        Self {
            storage: vec![0u64; word_capacity].into_boxed_slice(),
            len: 0,
        }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        let cap = self.capacity_bytes();
        let ptr = self.storage.as_mut_ptr() as *mut u8;
        unsafe { std::slice::from_raw_parts_mut(ptr, cap) }
    }

    pub(crate) fn capacity_bytes(&self) -> usize {
        self.storage.len() * std::mem::size_of::<u64>()
    }

    pub(crate) fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.capacity_bytes());
        self.len = len;
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        let ptr = self.storage.as_ptr() as *const u8;
        unsafe { std::slice::from_raw_parts(ptr, self.len) }
    }

    /// Copy `src` into a freshly-allocated aligned buffer.
    pub(crate) fn copy_from(src: &[u8]) -> Self {
        let mut buf = Self::with_capacity(src.len());
        let cap = buf.capacity_bytes();
        let dst = {
            let ptr = buf.storage.as_mut_ptr() as *mut u8;
            unsafe { std::slice::from_raw_parts_mut(ptr, cap) }
        };
        dst[..src.len()].copy_from_slice(src);
        buf.len = src.len();
        buf
    }
}

impl std::fmt::Debug for AlignedRequestBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlignedRequestBuf")
            .field("capacity", &self.capacity_bytes())
            .field("len", &self.len)
            .finish()
    }
}

/// Owned, thread-safe FUSE request data.
///
/// Owns the raw bytes so the session loop can push it onto a worker queue while immediately
/// reading the next kernel message.  Parsing of `ll::AnyRequest` happens on demand inside
/// `dispatch`, borrowing from the owned buffer for the duration of the call.
#[derive(Debug)]
pub struct Request {
    /// Channel sender for sending the reply
    ch: ChannelSender,
    /// Deferred notifier for enqueueing cache invalidations
    deferred: Arc<DeferredNotifier>,
    /// Request raw data (aligned, owned)
    data: AlignedRequestBuf,
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) enum ScheduleKey {
    FileHandle(u64),
    Inode(u64),
    Parent(u64),
    Pid(u64),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ScheduleClass {
    Keyed(ScheduleKey),
    GlobalWrite,
}

impl Request {
    /// Create a new request from the given (already-read) bytes, validating that the header
    /// parses correctly.
    pub(crate) fn new(
        ch: ChannelSender,
        deferred: Arc<DeferredNotifier>,
        data: AlignedRequestBuf,
    ) -> Option<Request> {
        if ll::AnyRequest::try_from(data.as_slice()).is_err() {
            error!("Failed to parse FUSE request header");
            return None;
        }
        Some(Self { ch, deferred, data })
    }

    /// Returns the deferred-cache-invalidation handle tied to this session.
    pub fn deferred_notifier(&self) -> &DeferredNotifier {
        &self.deferred
    }

    fn request(&self) -> ll::AnyRequest<'_> {
        ll::AnyRequest::try_from(self.data.as_slice())
            .expect("header validated at construction time")
    }

    pub fn notifier(&self) -> Notifier {
        Notifier::new(self.ch.for_notify())
    }

    pub(crate) fn schedule_class(&self) -> ScheduleClass {
        let parsed = self.request();
        let Ok(op) = parsed.operation() else {
            return ScheduleClass::GlobalWrite;
        };
        let pid_key = ScheduleKey::Pid(parsed.pid() as u64);

        match op {
            ll::Operation::Init(_)
            | ll::Operation::Destroy(_)
            | ll::Operation::BatchForget(_)
            | ll::Operation::Interrupt(_)
            | ll::Operation::NotifyReply(_)
            | ll::Operation::CuseInit(_) => ScheduleClass::GlobalWrite,
            #[cfg(target_os = "macos")]
            ll::Operation::SetVolName(_) => ScheduleClass::GlobalWrite,
            _ => ScheduleClass::Keyed(pid_key),
        }
    }

    /// Returns the unique identifier of this request
    #[inline]
    pub fn unique(&self) -> u64 {
        self.request().unique().into()
    }

    /// Returns the uid of this request
    #[inline]
    pub fn uid(&self) -> u32 {
        self.request().uid()
    }

    /// Returns the gid of this request
    #[inline]
    pub fn gid(&self) -> u32 {
        self.request().gid()
    }

    /// Returns the pid of this request
    #[inline]
    pub fn pid(&self) -> u32 {
        self.request().pid()
    }

    /// Dispatch request to the given filesystem. This calls the appropriate filesystem
    /// operation method for the request and sends back the returned reply to the kernel.
    ///
    /// The `shared` handle carries session-wide atomic state (init/destroy flags, protocol
    /// versions) safe to touch from any worker thread.
    pub(crate) fn dispatch<FS: Filesystem>(&self, shared: &SessionShared<FS>) {
        let parsed = self.request();
        debug!("{}", parsed);
        let unique = parsed.unique();
        let started = std::time::Instant::now();
        let op_slot = parsed.operation().ok().map(|op| fuse_op_slot(&op));

        let res = match self.dispatch_req(shared, &parsed) {
            Ok(Some(resp)) => resp,
            Ok(None) => {
                if let Some(slot) = op_slot {
                    agentfs_sdk::profiling::record_fuse_op(slot, started.elapsed());
                }
                return;
            }
            Err(errno) => parsed.reply_err(errno),
        }
        .with_iovec(unique, |iov| self.ch.send(iov));
        if let Some(slot) = op_slot {
            agentfs_sdk::profiling::record_fuse_op(slot, started.elapsed());
        }

        if let Err(err) = res {
            warn!("Request {unique:?}: Failed to send reply: {err}");
        }
    }

    fn dispatch_req<'a, FS: Filesystem>(
        &self,
        shared: &SessionShared<FS>,
        parsed: &ll::AnyRequest<'a>,
    ) -> Result<Option<Response<'a>>, Errno> {
        let op = parsed.operation().map_err(|_| Errno::ENOSYS)?;
        // Implement allow_root & access check for auto_unmount
        if (shared.allowed == SessionACL::RootAndOwner
            && parsed.uid() != shared.session_owner
            && parsed.uid() != 0)
            || (shared.allowed == SessionACL::Owner && parsed.uid() != shared.session_owner)
        {
            match op {
                // Only allow operations that the kernel may issue without a uid set
                ll::Operation::Init(_)
                | ll::Operation::Destroy(_)
                | ll::Operation::Read(_)
                | ll::Operation::ReadDir(_)
                | ll::Operation::ReadDirPlus(_)
                | ll::Operation::BatchForget(_)
                | ll::Operation::Forget(_)
                | ll::Operation::Write(_)
                | ll::Operation::FSync(_)
                | ll::Operation::FSyncDir(_)
                | ll::Operation::Release(_)
                | ll::Operation::ReleaseDir(_) => {}
                _ => {
                    return Err(Errno::EACCES);
                }
            }
        }
        match op {
            // Filesystem initialization
            ll::Operation::Init(x) => {
                // We don't support ABI versions before 7.6
                let v = x.version();
                if v < ll::Version(7, 6) {
                    error!("Unsupported FUSE ABI version {v}");
                    return Err(Errno::EPROTO);
                }
                // Remember ABI version supported by kernel
                shared.set_proto_version(v.major(), v.minor());

                let mut config = KernelConfig::new(x.capabilities(), x.max_readahead());
                // Call filesystem init method and give it a chance to return an error
                shared
                    .filesystem
                    .init(self, &mut config)
                    .map_err(Errno::from_i32)?;

                // Reply with our desired version and settings. If the kernel supports a
                // larger major version, it'll re-send a matching init message. If it
                // supports only lower major versions, we replied with an error above.
                debug!(
                    "INIT response: ABI {}.{}, flags {:#x}, max readahead {}, max write {}",
                    abi::FUSE_KERNEL_VERSION,
                    abi::FUSE_KERNEL_MINOR_VERSION,
                    x.capabilities() & config.requested,
                    config.max_readahead,
                    config.max_write
                );
                shared.set_initialized(true);
                return Ok(Some(x.reply(&config)));
            }
            // Any operation is invalid before initialization
            _ if !shared.is_initialized() => {
                warn!("Ignoring FUSE operation before init: {}", parsed);
                return Err(Errno::EIO);
            }
            // Filesystem destroyed
            ll::Operation::Destroy(x) => {
                shared.filesystem.destroy();
                shared.set_destroyed(true);
                return Ok(Some(x.reply()));
            }
            // Any operation is invalid after destroy
            _ if shared.is_destroyed() => {
                warn!("Ignoring FUSE operation after destroy: {}", parsed);
                return Err(Errno::EIO);
            }

            ll::Operation::Interrupt(_) => {
                // TODO: handle FUSE_INTERRUPT
                return Err(Errno::ENOSYS);
            }

            ll::Operation::Lookup(x) => {
                shared.filesystem.lookup(
                    self,
                    parsed.nodeid().into(),
                    x.name().as_ref(),
                    self.reply(),
                );
            }
            ll::Operation::Forget(x) => {
                shared
                    .filesystem
                    .forget(self, parsed.nodeid().into(), x.nlookup()); // no reply
            }
            ll::Operation::GetAttr(_attr) => {
                shared.filesystem.getattr(
                    self,
                    parsed.nodeid().into(),
                    _attr.file_handle().map(std::convert::Into::into),
                    self.reply(),
                );
            }
            ll::Operation::SetAttr(x) => {
                shared.filesystem.setattr(
                    self,
                    parsed.nodeid().into(),
                    x.mode(),
                    x.uid(),
                    x.gid(),
                    x.size(),
                    x.atime(),
                    x.mtime(),
                    x.ctime(),
                    x.file_handle().map(std::convert::Into::into),
                    x.crtime(),
                    x.chgtime(),
                    x.bkuptime(),
                    x.flags(),
                    self.reply(),
                );
            }
            ll::Operation::ReadLink(_) => {
                shared
                    .filesystem
                    .readlink(self, parsed.nodeid().into(), self.reply());
            }
            ll::Operation::MkNod(x) => {
                shared.filesystem.mknod(
                    self,
                    parsed.nodeid().into(),
                    x.name().as_ref(),
                    x.mode(),
                    x.umask(),
                    x.rdev(),
                    self.reply(),
                );
            }
            ll::Operation::MkDir(x) => {
                shared.filesystem.mkdir(
                    self,
                    parsed.nodeid().into(),
                    x.name().as_ref(),
                    x.mode(),
                    x.umask(),
                    self.reply(),
                );
            }
            ll::Operation::Unlink(x) => {
                shared.filesystem.unlink(
                    self,
                    parsed.nodeid().into(),
                    x.name().as_ref(),
                    self.reply(),
                );
            }
            ll::Operation::RmDir(x) => {
                shared.filesystem.rmdir(
                    self,
                    parsed.nodeid().into(),
                    x.name().as_ref(),
                    self.reply(),
                );
            }
            ll::Operation::SymLink(x) => {
                shared.filesystem.symlink(
                    self,
                    parsed.nodeid().into(),
                    x.link_name().as_ref(),
                    Path::new(x.target()),
                    self.reply(),
                );
            }
            ll::Operation::Rename(x) => {
                shared.filesystem.rename(
                    self,
                    parsed.nodeid().into(),
                    x.src().name.as_ref(),
                    x.dest().dir.into(),
                    x.dest().name.as_ref(),
                    0,
                    self.reply(),
                );
            }
            ll::Operation::Link(x) => {
                shared.filesystem.link(
                    self,
                    x.inode_no().into(),
                    parsed.nodeid().into(),
                    x.dest().name.as_ref(),
                    self.reply(),
                );
            }
            ll::Operation::Open(x) => {
                shared
                    .filesystem
                    .open(self, parsed.nodeid().into(), x.flags(), self.reply());
            }
            ll::Operation::Read(x) => {
                shared.filesystem.read(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.size(),
                    x.flags(),
                    x.lock_owner().map(std::convert::Into::into),
                    self.reply(),
                );
            }
            ll::Operation::Write(x) => {
                shared.filesystem.write(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.data(),
                    x.write_flags(),
                    x.flags(),
                    x.lock_owner().map(std::convert::Into::into),
                    self.reply(),
                );
            }
            ll::Operation::Flush(x) => {
                shared.filesystem.flush(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    self.reply(),
                );
            }
            ll::Operation::Release(x) => {
                shared.filesystem.release(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.flags(),
                    x.lock_owner().map(std::convert::Into::into),
                    x.flush(),
                    self.reply(),
                );
            }
            ll::Operation::FSync(x) => {
                shared.filesystem.fsync(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.fdatasync(),
                    self.reply(),
                );
            }
            ll::Operation::OpenDir(x) => {
                shared
                    .filesystem
                    .opendir(self, parsed.nodeid().into(), x.flags(), self.reply());
            }
            ll::Operation::ReadDir(x) => {
                shared.filesystem.readdir(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    ReplyDirectory::new(parsed.unique().into(), self.ch.clone(), x.size() as usize),
                );
            }
            ll::Operation::ReleaseDir(x) => {
                shared.filesystem.releasedir(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.flags(),
                    self.reply(),
                );
            }
            ll::Operation::FSyncDir(x) => {
                shared.filesystem.fsyncdir(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.fdatasync(),
                    self.reply(),
                );
            }
            ll::Operation::StatFs(_) => {
                shared
                    .filesystem
                    .statfs(self, parsed.nodeid().into(), self.reply());
            }
            ll::Operation::SetXAttr(x) => {
                shared.filesystem.setxattr(
                    self,
                    parsed.nodeid().into(),
                    x.name(),
                    x.value(),
                    x.flags(),
                    x.position(),
                    self.reply(),
                );
            }
            ll::Operation::GetXAttr(x) => {
                shared.filesystem.getxattr(
                    self,
                    parsed.nodeid().into(),
                    x.name(),
                    x.size_u32(),
                    self.reply(),
                );
            }
            ll::Operation::ListXAttr(x) => {
                shared
                    .filesystem
                    .listxattr(self, parsed.nodeid().into(), x.size(), self.reply());
            }
            ll::Operation::RemoveXAttr(x) => {
                shared
                    .filesystem
                    .removexattr(self, parsed.nodeid().into(), x.name(), self.reply());
            }
            ll::Operation::Access(x) => {
                shared
                    .filesystem
                    .access(self, parsed.nodeid().into(), x.mask(), self.reply());
            }
            ll::Operation::Create(x) => {
                shared.filesystem.create(
                    self,
                    parsed.nodeid().into(),
                    x.name().as_ref(),
                    x.mode(),
                    x.umask(),
                    x.flags(),
                    self.reply(),
                );
            }
            ll::Operation::GetLk(x) => {
                shared.filesystem.getlk(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    x.lock().range.0,
                    x.lock().range.1,
                    x.lock().typ,
                    x.lock().pid,
                    self.reply(),
                );
            }
            ll::Operation::SetLk(x) => {
                shared.filesystem.setlk(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    x.lock().range.0,
                    x.lock().range.1,
                    x.lock().typ,
                    x.lock().pid,
                    false,
                    self.reply(),
                );
            }
            ll::Operation::SetLkW(x) => {
                shared.filesystem.setlk(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    x.lock().range.0,
                    x.lock().range.1,
                    x.lock().typ,
                    x.lock().pid,
                    true,
                    self.reply(),
                );
            }
            ll::Operation::BMap(x) => {
                shared.filesystem.bmap(
                    self,
                    parsed.nodeid().into(),
                    x.block_size(),
                    x.block(),
                    self.reply(),
                );
            }

            ll::Operation::IoCtl(x) => {
                if x.unrestricted() {
                    return Err(Errno::ENOSYS);
                }
                shared.filesystem.ioctl(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.flags(),
                    x.command(),
                    x.in_data(),
                    x.out_size(),
                    self.reply(),
                );
            }
            ll::Operation::Poll(x) => {
                let ph = PollHandle::new(self.ch.for_notify(), x.kernel_handle());

                shared.filesystem.poll(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    ph,
                    x.events(),
                    x.flags(),
                    self.reply(),
                );
            }
            ll::Operation::NotifyReply(_) => {
                // TODO: handle FUSE_NOTIFY_REPLY
                return Err(Errno::ENOSYS);
            }
            ll::Operation::BatchForget(x) => {
                shared.filesystem.batch_forget(self, x.nodes()); // no reply
            }
            ll::Operation::FAllocate(x) => {
                shared.filesystem.fallocate(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.len(),
                    x.mode(),
                    self.reply(),
                );
            }
            ll::Operation::ReadDirPlus(x) => {
                shared.filesystem.readdirplus(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    ReplyDirectoryPlus::new(
                        parsed.unique().into(),
                        self.ch.clone(),
                        x.size() as usize,
                    ),
                );
            }
            ll::Operation::Rename2(x) => {
                shared.filesystem.rename(
                    self,
                    x.from().dir.into(),
                    x.from().name.as_ref(),
                    x.to().dir.into(),
                    x.to().name.as_ref(),
                    x.flags(),
                    self.reply(),
                );
            }
            ll::Operation::Lseek(x) => {
                shared.filesystem.lseek(
                    self,
                    parsed.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.whence(),
                    self.reply(),
                );
            }
            ll::Operation::CopyFileRange(x) => {
                let (i, o) = (x.src(), x.dest());
                shared.filesystem.copy_file_range(
                    self,
                    i.inode.into(),
                    i.file_handle.into(),
                    i.offset,
                    o.inode.into(),
                    o.file_handle.into(),
                    o.offset,
                    x.len(),
                    x.flags().try_into().unwrap(),
                    self.reply(),
                );
            }
            #[cfg(target_os = "macos")]
            ll::Operation::SetVolName(x) => {
                shared.filesystem.setvolname(self, x.name(), self.reply());
            }
            #[cfg(target_os = "macos")]
            ll::Operation::GetXTimes(x) => {
                shared
                    .filesystem
                    .getxtimes(self, x.nodeid().into(), self.reply());
            }
            #[cfg(target_os = "macos")]
            ll::Operation::Exchange(x) => {
                shared.filesystem.exchange(
                    self,
                    x.from().dir.into(),
                    x.from().name.as_ref(),
                    x.to().dir.into(),
                    x.to().name.as_ref(),
                    x.options(),
                    self.reply(),
                );
            }

            ll::Operation::CuseInit(_) => {
                // TODO: handle CUSE_INIT
                return Err(Errno::ENOSYS);
            }
        }
        Ok(None)
    }

    /// Create a reply object for this request that can be passed to the filesystem
    /// implementation and makes sure that a request is replied exactly once
    fn reply<T: Reply>(&self) -> T {
        Reply::new(self.request().unique().into(), self.ch.clone())
    }
}
