use crate::transport::FileAttr;
use agentfs_core::Stats;
use parking_lot::{Mutex, MutexGuard};
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Directory entry materialized for readdir/readdirplus cache replies.
pub(super) struct CachedDirEntry {
    pub(super) name: String,
    pub(super) attr: FileAttr,
}

/// Freshness fingerprint for FOPEN_KEEP_CACHE eligibility.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct KeepCacheFingerprint {
    mode: u32,
    size: i64,
    mtime: i64,
    mtime_nsec: u32,
    ctime: i64,
    ctime_nsec: u32,
    rdev: u64,
}

impl KeepCacheFingerprint {
    pub(super) fn from_stats(stats: &Stats) -> Self {
        Self {
            mode: stats.mode,
            size: stats.size,
            mtime: stats.mtime,
            mtime_nsec: stats.mtime_nsec,
            ctime: stats.ctime,
            ctime_nsec: stats.ctime_nsec,
            rdev: stats.rdev,
        }
    }
}

/// Guards `FOPEN_KEEP_CACHE` grants against serving stale kernel pages.
///
/// Non-sticky (default): a mutation drops the stored fingerprint and the next
/// read-only open revalidates against fresh stats. This is sound because
/// every mutation path is kernel-originated: the kernel's own pages stay
/// coherent for its own writes, and adapter-notified invalidations purge them.
#[derive(Debug, Default)]
struct KeepCacheDriftGuard {
    sticky: bool,
    dropped: HashSet<u64>,
    fingerprints: HashMap<u64, KeepCacheFingerprint>,
}

impl KeepCacheDriftGuard {
    fn new(sticky: bool) -> Self {
        Self {
            sticky,
            ..Self::default()
        }
    }

    fn allows(&self, ino: u64, fingerprint: &KeepCacheFingerprint) -> bool {
        !self.dropped.contains(&ino)
            && self
                .fingerprints
                .get(&ino)
                .map(|existing| existing == fingerprint)
                .unwrap_or(true)
    }

    fn mark_eligible(&mut self, ino: u64, fingerprint: KeepCacheFingerprint) {
        if !self.dropped.contains(&ino) {
            self.fingerprints.insert(ino, fingerprint);
        }
    }

    fn drop_eligibility(&mut self, ino: u64) -> bool {
        let had_fingerprint = self.fingerprints.remove(&ino).is_some();
        if self.sticky {
            self.dropped.insert(ino) || had_fingerprint
        } else {
            had_fingerprint
        }
    }
}

/// Whether a cache mutation should also notify the kernel.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum NotifyPolicy {
    NotifyKernel,
    SuppressKernel,
}

/// Reply-lock guard retained while a cacheable FUSE reply is emitted.
pub(super) struct CacheReplyGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

/// Adapter-owned kernel-cache coherence state.
///
/// All epoch checks and mutation invalidations pass through this API so cache
/// reply locks are never held across backend `block_on` calls. Callers perform
/// async work first, then acquire [`CacheReplyGuard`] or call [`Self::mutate`].
pub(super) struct AdapterCaches {
    dir_entries: Arc<Mutex<HashMap<u64, Arc<Vec<CachedDirEntry>>>>>,
    attr: Arc<Mutex<HashMap<u64, Stats>>>,
    entry: Arc<Mutex<HashMap<(u64, String), Stats>>>,
    negative_entry: Arc<Mutex<HashMap<(u64, String), ()>>>,
    external_read_fingerprints: Arc<Mutex<HashMap<u64, KeepCacheFingerprint>>>,
    external_read_seen: Arc<Mutex<HashSet<u64>>>,
    keepcache_drift: Arc<Mutex<KeepCacheDriftGuard>>,
    reply_lock: Arc<Mutex<()>>,
    epoch: AtomicU64,
}

impl AdapterCaches {
    pub(super) fn new(keepcache_sticky_drop: bool) -> Self {
        Self {
            dir_entries: Arc::new(Mutex::new(HashMap::new())),
            attr: Arc::new(Mutex::new(HashMap::new())),
            entry: Arc::new(Mutex::new(HashMap::new())),
            negative_entry: Arc::new(Mutex::new(HashMap::new())),
            external_read_fingerprints: Arc::new(Mutex::new(HashMap::new())),
            external_read_seen: Arc::new(Mutex::new(HashSet::new())),
            keepcache_drift: Arc::new(Mutex::new(KeepCacheDriftGuard::new(keepcache_sticky_drop))),
            reply_lock: Arc::new(Mutex::new(())),
            epoch: AtomicU64::new(0),
        }
    }

    pub(super) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub(super) fn epoch_changed(&self, epoch: u64) -> bool {
        self.epoch.load(Ordering::Acquire) != epoch
    }

    pub(super) fn try_reply_guard(&self, snapshot_epoch: u64) -> Option<CacheReplyGuard<'_>> {
        let guard = self.reply_lock.try_lock()?;
        if self.epoch_changed(snapshot_epoch) {
            return None;
        }
        Some(CacheReplyGuard { _guard: guard })
    }

    pub(super) fn mutate(&self, _policy: NotifyPolicy, f: impl FnOnce(&Self)) {
        let _reply = self.reply_lock.lock();
        self.epoch.fetch_add(1, Ordering::AcqRel);
        f(self);
    }

    pub(super) fn invalidate_inode(&self, ino: u64, policy: NotifyPolicy) -> bool {
        let mut dropped_keepcache = false;
        self.mutate(policy, |caches| {
            dropped_keepcache = caches.drop_keepcache_eligibility(ino);
            caches.attrs().lock().remove(&ino);
            caches
                .entries()
                .lock()
                .retain(|_, stats| stats.ino as u64 != ino);
            caches.dir_entries().lock().retain(|dir_ino, entries| {
                *dir_ino != ino && !entries.iter().any(|entry| entry.attr.ino == ino)
            });
        });
        dropped_keepcache
    }

    pub(super) fn invalidate_entry(&self, parent: u64, name: &str, policy: NotifyPolicy) -> bool {
        let mut removed_negative = false;
        self.mutate(policy, |caches| {
            let key = (parent, name.to_string());
            caches.entries().lock().remove(&key);
            removed_negative = caches.negative_entries().lock().remove(&key).is_some();
        });
        removed_negative
    }

    pub(super) fn dir_entries(&self) -> &Mutex<HashMap<u64, Arc<Vec<CachedDirEntry>>>> {
        &self.dir_entries
    }

    pub(super) fn attrs(&self) -> &Mutex<HashMap<u64, Stats>> {
        &self.attr
    }

    pub(super) fn entries(&self) -> &Mutex<HashMap<(u64, String), Stats>> {
        &self.entry
    }

    pub(super) fn negative_entries(&self) -> &Mutex<HashMap<(u64, String), ()>> {
        &self.negative_entry
    }

    pub(super) fn keepcache_allows(&self, ino: u64, fingerprint: &KeepCacheFingerprint) -> bool {
        self.keepcache_drift.lock().allows(ino, fingerprint)
    }

    pub(super) fn mark_keepcache_eligible(&self, ino: u64, fingerprint: KeepCacheFingerprint) {
        self.keepcache_drift.lock().mark_eligible(ino, fingerprint);
    }

    pub(super) fn drop_keepcache_eligibility(&self, ino: u64) -> bool {
        self.keepcache_drift.lock().drop_eligibility(ino)
    }

    pub(super) fn set_external_read_fingerprint(
        &self,
        ino: u64,
        fingerprint: KeepCacheFingerprint,
    ) {
        self.external_read_fingerprints
            .lock()
            .insert(ino, fingerprint);
    }

    pub(super) fn mark_external_read_seen(&self, ino: u64) {
        self.external_read_seen.lock().insert(ino);
    }

    pub(super) fn external_read_was_seen(&self, ino: u64) -> bool {
        self.external_read_seen.lock().contains(&ino)
    }

    pub(super) fn external_read_drifted(
        &self,
        ino: u64,
        fingerprint: KeepCacheFingerprint,
    ) -> bool {
        let mut fingerprints = self.external_read_fingerprints.lock();
        match fingerprints.get(&ino) {
            Some(existing) => existing != &fingerprint,
            None => {
                fingerprints.insert(ino, fingerprint);
                false
            }
        }
    }
}
