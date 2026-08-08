//! Adapter-side kernel cache coherence state (`AdapterCaches`).
//!
//! Lock order: `reply_lock` -> one cache map lock at a time (`dir_entries`,
//! `attr`, `entry`, `negative_entry`, `external_read_*`, `keepcache_drift`).
//! `external_kernel` participates at the same one-map-at-a-time level.
//! Mutations go through [`AdapterCaches::mutate`], which holds `reply_lock`
//! while bumping the epoch and touching map locks sequentially; cacheable
//! replies hold only [`CacheReplyGuard`]. Map locks never nest with each
//! other, and no guard is held across backend `block_on`/`.await` calls
//! (`clippy::await_holding_lock` is deny-by-workspace).

use crate::transport::FileAttr;
use parking_lot::{Mutex, MutexGuard};
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use vfs_core::Stats;

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

impl NotifyPolicy {
    fn should_notify_kernel(self) -> bool {
        matches!(self, Self::NotifyKernel)
    }
}

pub(super) struct InodeInvalidation {
    pub(super) dropped_keepcache: bool,
    pub(super) notify_kernel: bool,
}

pub(super) struct EntryInvalidation {
    pub(super) removed_negative: bool,
    pub(super) notify_kernel: bool,
}

pub(super) struct ExternalKernelInvalidation {
    pub(super) inodes: Vec<u64>,
    pub(super) entries: Vec<(u64, String)>,
}

#[derive(Default)]
struct ExternalKernelCache {
    inodes: HashSet<u64>,
    entries: HashSet<(u64, String, u64)>,
    lookups: HashMap<u64, u64>,
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
    external_kernel: Arc<Mutex<ExternalKernelCache>>,
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
            external_kernel: Arc::new(Mutex::new(ExternalKernelCache::default())),
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

    fn mutate(&self, policy: NotifyPolicy, f: impl FnOnce(&Self)) -> bool {
        let _reply = self.reply_lock.lock();
        self.epoch.fetch_add(1, Ordering::AcqRel);
        f(self);
        policy.should_notify_kernel()
    }

    pub(super) fn invalidate_inode(&self, ino: u64, policy: NotifyPolicy) -> InodeInvalidation {
        let mut dropped_keepcache = false;
        let notify_kernel = self.mutate(policy, |caches| {
            dropped_keepcache = caches.drop_keepcache_eligibility(ino);
            caches.prune_external_read_state(ino);
            let mut external = caches.external_kernel.lock();
            external.inodes.remove(&ino);
            external.lookups.remove(&ino);
            external
                .entries
                .retain(|(_, _, entry_ino)| *entry_ino != ino);
            drop(external);
            caches.attrs().lock().remove(&ino);
            caches
                .entries()
                .lock()
                .retain(|_, stats| stats.ino as u64 != ino);
            caches.dir_entries().lock().retain(|dir_ino, entries| {
                *dir_ino != ino && !entries.iter().any(|entry| entry.attr.ino == ino)
            });
        });
        InodeInvalidation {
            dropped_keepcache,
            notify_kernel,
        }
    }

    pub(super) fn invalidate_entry(
        &self,
        parent: u64,
        name: &str,
        policy: NotifyPolicy,
    ) -> EntryInvalidation {
        let mut removed_negative = false;
        let notify_kernel = self.mutate(policy, |caches| {
            let key = (parent, name.to_string());
            caches.entries().lock().remove(&key);
            removed_negative = caches.negative_entries().lock().remove(&key).is_some();
            caches
                .external_kernel
                .lock()
                .entries
                .retain(|(entry_parent, entry_name, _)| {
                    *entry_parent != parent || entry_name != name
                });
        });
        EntryInvalidation {
            removed_negative,
            notify_kernel,
        }
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

    pub(super) fn prune_external_read_state(&self, ino: u64) {
        self.external_read_fingerprints.lock().remove(&ino);
        self.external_read_seen.lock().remove(&ino);
    }

    pub(super) fn external_read_drifted(
        &self,
        ino: u64,
        fingerprint: KeepCacheFingerprint,
    ) -> bool {
        let mut fingerprints = self.external_read_fingerprints.lock();
        match fingerprints.get(&ino) {
            Some(existing) if existing != &fingerprint => {
                fingerprints.insert(ino, fingerprint);
                true
            }
            Some(_) => false,
            None => {
                fingerprints.insert(ino, fingerprint);
                false
            }
        }
    }

    /// Record an external-origin inode granted a non-zero kernel metadata TTL.
    ///
    /// Callers hold a [`CacheReplyGuard`] while recording and replying, so an
    /// out-of-band invalidation cannot interleave between these two actions.
    pub(super) fn track_external_kernel_inode(&self, ino: u64) {
        self.external_kernel.lock().inodes.insert(ino);
    }

    /// Record an external-origin dentry granted a non-zero kernel entry TTL.
    ///
    /// Callers hold a [`CacheReplyGuard`] while recording and replying.
    pub(super) fn track_external_kernel_lookup(&self, parent: u64, name: Option<&str>, ino: u64) {
        let mut external = self.external_kernel.lock();
        external.inodes.insert(ino);
        if let Some(name) = name {
            external.entries.insert((parent, name.to_string(), ino));
        }
        let refs = external.lookups.entry(ino).or_insert(0);
        *refs = refs.saturating_add(1);
    }

    /// Forget tracked external kernel grants once the kernel drops its final
    /// lookup reference for the inode.
    pub(super) fn forget_external_kernel_lookup(&self, ino: u64, nlookup: u64) {
        let mut external = self.external_kernel.lock();
        let final_lookup = {
            let Some(refs) = external.lookups.get_mut(&ino) else {
                return;
            };
            if *refs > nlookup {
                *refs -= nlookup;
                false
            } else {
                external.lookups.remove(&ino);
                true
            }
        };
        if final_lookup {
            external.inodes.remove(&ino);
            external
                .entries
                .retain(|(_, _, entry_ino)| *entry_ino != ino);
        }
    }

    /// Drop every kernel cache grant whose backing host state may have changed.
    ///
    /// The epoch bump makes in-flight backend observations retry or reply with
    /// zero TTL. Negative dentries are included because a host-side create can
    /// make any previously missing name visible.
    pub(super) fn invalidate_external_kernel_state(&self) -> ExternalKernelInvalidation {
        let mut inodes = Vec::new();
        let mut entries = Vec::new();
        self.mutate(NotifyPolicy::SuppressKernel, |caches| {
            let mut external = caches.external_kernel.lock();
            let invalidated = std::mem::take(&mut external.inodes);
            entries.extend(
                external
                    .entries
                    .drain()
                    .map(|(parent, name, _)| (parent, name)),
            );
            drop(external);
            entries.extend(
                caches
                    .negative_entries()
                    .lock()
                    .drain()
                    .map(|(entry, ())| entry),
            );

            {
                let mut keepcache = caches.keepcache_drift.lock();
                for ino in &invalidated {
                    keepcache.drop_eligibility(*ino);
                }
            }
            caches
                .external_read_fingerprints
                .lock()
                .retain(|ino, _| !invalidated.contains(ino));
            caches
                .external_read_seen
                .lock()
                .retain(|ino| !invalidated.contains(ino));
            caches
                .attrs()
                .lock()
                .retain(|ino, _| !invalidated.contains(ino));
            caches
                .entries()
                .lock()
                .retain(|_, stats| !invalidated.contains(&(stats.ino as u64)));
            caches.dir_entries().lock().retain(|dir_ino, dir_entries| {
                !invalidated.contains(dir_ino)
                    && !dir_entries
                        .iter()
                        .any(|entry| invalidated.contains(&entry.attr.ino))
            });
            inodes.extend(invalidated);
        });
        entries.sort();
        entries.dedup();
        ExternalKernelInvalidation { inodes, entries }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_with_size(size: i64) -> Stats {
        Stats {
            ino: 42,
            mode: libc::S_IFREG | 0o644,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            size,
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
    fn notify_policy_controls_mutation_notify_flag() {
        let caches = AdapterCaches::new(false);

        assert!(caches.mutate(NotifyPolicy::NotifyKernel, |_| {}));
        assert!(!caches.mutate(NotifyPolicy::SuppressKernel, |_| {}));
    }

    #[test]
    fn external_read_drift_revalidates_after_one_error() {
        let caches = AdapterCaches::new(false);
        let ino = 7;
        caches.mark_external_read_seen(ino);

        let before = KeepCacheFingerprint::from_stats(&stats_with_size(5));
        let after = KeepCacheFingerprint::from_stats(&stats_with_size(9));

        assert!(
            !caches.external_read_drifted(ino, before.clone()),
            "first fingerprint records the baseline"
        );
        assert!(
            caches.external_read_drifted(ino, after.clone()),
            "changed fingerprint reports one drift error"
        );
        assert!(
            !caches.external_read_drifted(ino, after),
            "the changed fingerprint becomes the new validated baseline"
        );
    }

    #[test]
    fn prune_external_read_state_removes_seen_and_fingerprint() {
        let caches = AdapterCaches::new(false);
        let ino = 11;
        let fingerprint = KeepCacheFingerprint::from_stats(&stats_with_size(5));

        caches.mark_external_read_seen(ino);
        caches.set_external_read_fingerprint(ino, fingerprint.clone());
        assert!(caches.external_read_was_seen(ino));

        caches.prune_external_read_state(ino);

        assert!(!caches.external_read_was_seen(ino));
        assert!(
            !caches.external_read_drifted(ino, fingerprint),
            "pruned fingerprint should behave like a fresh baseline"
        );
    }

    #[test]
    fn external_mutation_drains_positive_and_negative_kernel_grants() {
        let caches = AdapterCaches::new(false);
        caches.track_external_kernel_inode(11);
        caches.track_external_kernel_lookup(1, Some("present"), 11);
        caches
            .negative_entries()
            .lock()
            .insert((1, "missing".to_string()), ());
        caches.mark_external_read_seen(11);
        caches.set_external_read_fingerprint(
            11,
            KeepCacheFingerprint::from_stats(&stats_with_size(5)),
        );
        let epoch = caches.epoch();

        let invalidation = caches.invalidate_external_kernel_state();

        assert_eq!(invalidation.inodes, vec![11]);
        assert_eq!(
            invalidation.entries,
            vec![(1, "missing".to_string()), (1, "present".to_string())]
        );
        assert!(caches.epoch_changed(epoch));
        assert!(!caches.external_read_was_seen(11));
        assert!(caches.invalidate_external_kernel_state().inodes.is_empty());
    }

    #[test]
    fn external_kernel_grants_live_until_the_final_forget() {
        let caches = AdapterCaches::new(false);
        caches.track_external_kernel_inode(11);
        caches.track_external_kernel_lookup(1, Some("present"), 11);
        caches.track_external_kernel_lookup(1, Some("present"), 11);

        caches.forget_external_kernel_lookup(11, 1);
        assert!(caches.external_kernel.lock().inodes.contains(&11));

        caches.forget_external_kernel_lookup(11, 1);
        let external = caches.external_kernel.lock();
        assert!(!external.inodes.contains(&11));
        assert!(external.entries.is_empty());
    }
}
