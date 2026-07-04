//! Handle and authority state shared by protocol adapters.
//!
//! NFS is stateless, but AgentFS deliberately preserves write authority
//! captured by a successful CREATE response so later mode changes do not make
//! already-open client writeback fail. This table is scoped to one
//! [`Semantics`](super::Semantics) facade.

use crate::error::Result;
use crate::fs::{agentfs::ReapHook, BoxedFile, FileSystem};
use async_trait::async_trait;
use lru::LruCache;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::Arc;
use turso::Connection;

const DEFAULT_WRITE_TOKEN_CAPACITY: usize = 16_384;
const DEFAULT_OPEN_HANDLE_CAPACITY: usize = 16_384;

/// Requested cached-handle authority.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Authority {
    /// Read-only data access.
    Read,
    /// Read/write data or truncate access.
    Write,
}

impl Authority {
    fn open_flags(self) -> i32 {
        match self {
            Self::Read => libc::O_RDONLY,
            Self::Write => libc::O_RDWR,
        }
    }
}

/// A cached open file handle owned by the semantics layer.
#[derive(Clone)]
pub struct Handle {
    ino: i64,
    authority: Authority,
    file: BoxedFile,
}

impl Handle {
    pub fn ino(&self) -> i64 {
        self.ino
    }

    pub fn authority(&self) -> Authority {
        self.authority
    }

    pub fn file(&self) -> &BoxedFile {
        &self.file
    }
}

#[derive(Clone, Debug)]
struct TokenEntry {
    ino: i64,
    last_used: u64,
}

#[derive(Clone)]
struct OpenEntry {
    handle: Handle,
    write_capable: bool,
}

struct HandleTableInner {
    tokens: LruCache<u64, TokenEntry>,
    tokens_by_ino: HashMap<i64, HashSet<u64>>,
    token_clock: u64,
    open_handles: LruCache<i64, OpenEntry>,
}

impl HandleTableInner {
    fn new(token_capacity: usize, open_capacity: usize) -> Self {
        Self {
            tokens: LruCache::new(
                NonZeroUsize::new(token_capacity).expect("token capacity must be non-zero"),
            ),
            tokens_by_ino: HashMap::new(),
            token_clock: 0,
            open_handles: LruCache::new(
                NonZeroUsize::new(open_capacity).expect("open capacity must be non-zero"),
            ),
        }
    }

    fn next_token_stamp(&mut self) -> u64 {
        let stamp = self.token_clock;
        self.token_clock = self.token_clock.wrapping_add(1);
        stamp
    }

    fn remove_token_from_index(&mut self, ino: i64, token: u64) {
        if let Some(tokens) = self.tokens_by_ino.get_mut(&ino) {
            tokens.remove(&token);
            if tokens.is_empty() {
                self.tokens_by_ino.remove(&ino);
            }
        }
    }

    fn insert_token(&mut self, ino: i64, token: u64) {
        let stamp = self.next_token_stamp();
        if let Some((evicted_token, evicted_entry)) = self.tokens.push(
            token,
            TokenEntry {
                ino,
                last_used: stamp,
            },
        ) {
            self.remove_token_from_index(evicted_entry.ino, evicted_token);
        }
        self.tokens_by_ino.entry(ino).or_default().insert(token);
    }

    fn touch_token(&mut self, token: u64) {
        let stamp = self.next_token_stamp();
        if let Some(entry) = self.tokens.get_mut(&token) {
            entry.last_used = stamp;
        }
    }

    fn invalidate_ino(&mut self, ino: i64) {
        if let Some(tokens) = self.tokens_by_ino.remove(&ino) {
            for token in tokens {
                self.tokens.pop(&token);
            }
        }

        self.open_handles.pop(&ino);
    }
}

/// Per-semantics table of write-authority tokens and cached open handles.
#[derive(Clone)]
pub struct HandleTable {
    inner: Arc<Mutex<HandleTableInner>>,
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::with_limits(DEFAULT_WRITE_TOKEN_CAPACITY, DEFAULT_OPEN_HANDLE_CAPACITY)
    }
}

impl HandleTable {
    pub fn with_limits(token_capacity: usize, open_capacity: usize) -> Self {
        assert!(token_capacity > 0, "token capacity must be non-zero");
        assert!(open_capacity > 0, "open capacity must be non-zero");
        Self {
            inner: Arc::new(Mutex::new(HandleTableInner::new(
                token_capacity,
                open_capacity,
            ))),
        }
    }

    /// Insert a server-generated authority token.
    ///
    /// Returns `false` when the token already exists in this table so the
    /// caller can retry with new randomness without replacing another handle's
    /// authority.
    pub fn try_grant_write_authority_with_token(&self, ino: i64, token: u64) -> bool {
        let mut inner = self.inner.lock();
        if inner.tokens.peek(&token).is_some() {
            return false;
        }
        inner.insert_token(ino, token);
        true
    }

    pub fn has_write_authority(&self, ino: i64, token: u64) -> bool {
        let mut inner = self.inner.lock();
        let has_authority = inner
            .tokens
            .peek(&token)
            .map(|entry| entry.ino == ino)
            .unwrap_or(false);
        if has_authority {
            inner.touch_token(token);
        }
        has_authority
    }

    /// Return a live authority token for `ino`, if one exists, and mark it
    /// recently used. READDIRPLUS uses this to refresh client node handles
    /// without stripping CREATE-captured write authority.
    pub fn authority_token_for_ino(&self, ino: i64) -> Option<u64> {
        let mut inner = self.inner.lock();
        let token = inner.tokens_by_ino.get(&ino).and_then(|tokens| {
            tokens
                .iter()
                .filter_map(|token| {
                    inner
                        .tokens
                        .peek(token)
                        .map(|entry| (*token, entry.last_used))
                })
                .max_by_key(|(_, last_used)| *last_used)
                .map(|(token, _)| token)
        });
        if let Some(token) = token {
            inner.touch_token(token);
        }
        token
    }

    /// Open and cache one handle per inode.
    ///
    /// A write request upgrades a read-resolved entry by replacing the cached
    /// file with an `O_RDWR` open. Overlay copy-up happens during that write
    /// open, so later reads must reuse the upgraded file rather than a stale
    /// base-layer read handle.
    pub async fn open_cached(
        &self,
        fs: &Arc<dyn FileSystem>,
        ino: i64,
        authority: Authority,
    ) -> Result<Handle> {
        let needs_write = matches!(authority, Authority::Write);
        {
            let mut inner = self.inner.lock();
            if let Some(entry) = inner.open_handles.get(&ino).cloned() {
                if !needs_write || entry.write_capable {
                    return Ok(entry.handle);
                }
            }
        }

        let file = fs.open(ino, authority.open_flags()).await?;
        let handle = Handle {
            ino,
            authority,
            file,
        };

        let mut inner = self.inner.lock();
        if let Some(entry) = inner.open_handles.get(&ino).cloned() {
            if !needs_write || entry.write_capable {
                return Ok(entry.handle);
            }
        }

        inner.open_handles.push(
            ino,
            OpenEntry {
                handle: handle.clone(),
                write_capable: needs_write,
            },
        );
        Ok(handle)
    }

    pub fn invalidate_ino(&self, ino: i64) {
        self.inner.lock().invalidate_ino(ino);
    }

    #[cfg(test)]
    fn grant_write_authority_with_token_for_test(&self, ino: i64, token: u64) -> u64 {
        assert!(self.try_grant_write_authority_with_token(ino, token));
        token
    }
}

#[async_trait]
impl ReapHook for HandleTable {
    async fn on_reap(&self, _conn: &Connection, ino: i64) -> Result<()> {
        self.invalidate_ino(ino);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentFS, AgentFSOptions, FileSystem, Semantics, DEFAULT_FILE_MODE};

    #[test]
    fn write_authority_tokens_evict_least_recently_used_token() {
        let table = HandleTable::with_limits(2, 2);

        let first = table.grant_write_authority_with_token_for_test(10, 101);
        let second = table.grant_write_authority_with_token_for_test(20, 202);
        assert!(table.has_write_authority(10, first));
        assert!(table.has_write_authority(20, second));

        // Touch the first token so the second token becomes the LRU victim.
        assert!(table.has_write_authority(10, first));
        let third = table.grant_write_authority_with_token_for_test(30, 303);

        assert!(table.has_write_authority(10, first));
        assert!(!table.has_write_authority(20, second));
        assert!(table.has_write_authority(30, third));
    }

    #[test]
    fn write_authority_tokens_are_invalidated_by_inode() {
        let table = HandleTable::with_limits(4, 4);

        let victim = table.grant_write_authority_with_token_for_test(10, 101);
        let unrelated = table.grant_write_authority_with_token_for_test(20, 202);
        table.invalidate_ino(10);

        assert!(!table.has_write_authority(10, victim));
        assert!(table.has_write_authority(20, unrelated));
    }

    #[test]
    fn readdirplus_can_reuse_a_live_write_authority_token() {
        let table = HandleTable::with_limits(4, 4);
        let token = table.grant_write_authority_with_token_for_test(10, 101);

        assert_eq!(table.authority_token_for_ino(10), Some(token));
        assert!(table.has_write_authority(10, token));
        assert_eq!(table.authority_token_for_ino(20), None);
    }

    #[tokio::test]
    async fn open_cached_reuses_and_invalidates_handles_by_inode() -> Result<()> {
        let agent = AgentFS::open(AgentFSOptions::ephemeral()).await?;
        let fs: Arc<dyn FileSystem> = Arc::new(agent.fs);
        let (stats, _file) = fs
            .create_file(1, "cached.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        let table = HandleTable::with_limits(4, 4);

        let first = table.open_cached(&fs, stats.ino, Authority::Read).await?;
        let second = table.open_cached(&fs, stats.ino, Authority::Read).await?;
        assert!(Arc::ptr_eq(first.file(), second.file()));

        table.invalidate_ino(stats.ino);
        let reopened = table.open_cached(&fs, stats.ino, Authority::Read).await?;
        assert!(!Arc::ptr_eq(first.file(), reopened.file()));
        Ok(())
    }

    #[tokio::test]
    async fn open_cached_upgrades_read_handle_on_first_write_open() -> Result<()> {
        let agent = AgentFS::open(AgentFSOptions::ephemeral()).await?;
        let fs: Arc<dyn FileSystem> = Arc::new(agent.fs);
        let (stats, _file) = fs
            .create_file(1, "upgrade.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        let table = HandleTable::with_limits(4, 4);

        let read = table.open_cached(&fs, stats.ino, Authority::Read).await?;
        assert_eq!(read.authority(), Authority::Read);

        let write = table.open_cached(&fs, stats.ino, Authority::Write).await?;
        assert_eq!(write.authority(), Authority::Write);
        assert!(
            !Arc::ptr_eq(read.file(), write.file()),
            "write open must replace a previously read-only cached file"
        );

        let reread = table.open_cached(&fs, stats.ino, Authority::Read).await?;
        assert!(
            Arc::ptr_eq(write.file(), reread.file()),
            "reads after a write upgrade must reuse the write-capable file"
        );
        Ok(())
    }

    #[tokio::test]
    async fn reap_hook_invalidates_authority_tokens() -> Result<()> {
        let agent = AgentFS::open(AgentFSOptions::ephemeral()).await?;
        let fs: Arc<dyn FileSystem> = Arc::new(agent.fs);
        let semantics = Semantics::new(fs.clone());
        let (stats, file) = fs
            .create_file(1, "reaped.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        drop(file);

        let token = 404;
        assert!(semantics.try_grant_write_authority_with_token(stats.ino, token));
        assert!(semantics.has_write_authority(stats.ino, token));

        fs.unlink(1, "reaped.txt").await?;

        assert!(!semantics.has_write_authority(stats.ino, token));
        Ok(())
    }
}
