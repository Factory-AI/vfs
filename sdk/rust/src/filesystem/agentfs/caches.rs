//! Bounded dentry, negative-dentry, and attr caches for AgentFS.
//!
//! The cache layer is intentionally synchronous and conservative: namespace
//! and metadata mutations invalidate affected keys before reporting success,
//! while lookups update profiling counters for hot-path visibility.

use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use super::Stats;

/// LRU cache for directory entry lookups.
///
/// Maps (parent_ino, name) -> child_ino to avoid repeated database queries
/// during path resolution. For a path like `/a/b/c/d`, this reduces queries
/// from 4 to potentially 0 on cache hits.
pub(super) struct DentryCache {
    // Mutex required because LruCache::get() mutates internal order
    entries: Mutex<LruCache<(i64, String), i64>>,
}

impl DentryCache {
    pub(super) fn new(max_size: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_size).expect("cache size must be > 0"),
            )),
        }
    }

    /// Look up a cached entry (updates LRU order)
    pub(super) fn get(&self, parent_ino: i64, name: &str) -> Option<i64> {
        let entry = self
            .entries
            .lock()
            .unwrap()
            .get(&(parent_ino, name.to_string()))
            .copied();
        if entry.is_some() {
            crate::profiling::record_dentry_cache_hit();
            crate::profiling::record_path_cache_hit();
        } else {
            crate::profiling::record_dentry_cache_miss();
            crate::profiling::record_path_cache_miss();
        }
        entry
    }

    /// Insert an entry into the cache (evicts LRU entry if full)
    pub(super) fn insert(&self, parent_ino: i64, name: &str, child_ino: i64) {
        self.entries
            .lock()
            .unwrap()
            .put((parent_ino, name.to_string()), child_ino);
    }

    /// Remove an entry from the cache
    pub(super) fn remove(&self, parent_ino: i64, name: &str) {
        self.entries
            .lock()
            .unwrap()
            .pop(&(parent_ino, name.to_string()));
    }
}

/// LRU cache for safe negative directory entry lookups.
///
/// A negative entry means "this (parent, name) did not exist in the last
/// serialized AgentFS view". Every namespace mutation invalidates exactly the
/// affected key before the mutation reports success, so cached ENOENT results
/// cannot hide later creates or renames made through this filesystem.
pub(super) struct NegativeDentryCache {
    entries: Mutex<LruCache<(i64, String), ()>>,
}

impl NegativeDentryCache {
    pub(super) fn new(max_size: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_size).expect("cache size must be > 0"),
            )),
        }
    }

    pub(super) fn contains(&self, parent_ino: i64, name: &str) -> bool {
        let cached = self
            .entries
            .lock()
            .unwrap()
            .get(&(parent_ino, name.to_string()))
            .is_some();
        if cached {
            crate::profiling::record_negative_cache_hit();
        } else {
            crate::profiling::record_negative_cache_miss();
        }
        cached
    }

    pub(super) fn insert(&self, parent_ino: i64, name: &str) {
        self.entries
            .lock()
            .unwrap()
            .put((parent_ino, name.to_string()), ());
    }

    pub(super) fn remove(&self, parent_ino: i64, name: &str) {
        if self
            .entries
            .lock()
            .unwrap()
            .pop(&(parent_ino, name.to_string()))
            .is_some()
        {
            crate::profiling::record_negative_cache_invalidation();
        }
    }
}

/// LRU cache for inode attributes.
///
/// FUSE and SDK stat-heavy read paths often ask for the same inode metadata
/// repeatedly after lookup/readdir_plus. This cache is conservative: every
/// namespace, metadata, or size/content mutation invalidates the affected inode
/// and parent directory entries before the mutation is considered complete.
pub(super) struct AttrCache {
    entries: Mutex<LruCache<i64, Stats>>,
}

impl AttrCache {
    pub(super) fn new(max_size: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_size).expect("cache size must be > 0"),
            )),
        }
    }

    pub(super) fn get(&self, ino: i64) -> Option<Stats> {
        let stats = self.entries.lock().unwrap().get(&ino).cloned();
        if stats.is_some() {
            crate::profiling::record_attr_cache_hit();
        } else {
            crate::profiling::record_attr_cache_miss();
        }
        stats
    }

    pub(super) fn insert(&self, stats: Stats) {
        self.entries.lock().unwrap().put(stats.ino, stats);
    }

    pub(super) fn remove(&self, ino: i64) {
        self.entries.lock().unwrap().pop(&ino);
    }
}
