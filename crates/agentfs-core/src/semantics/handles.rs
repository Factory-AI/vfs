//! Handle and authority state shared by protocol adapters.
//!
//! NFS is stateless, but AgentFS deliberately preserves write authority
//! captured by a successful CREATE response so later mode changes do not make
//! already-open client writeback fail. This table is scoped to one
//! [`Semantics`](super::Semantics) facade.

use crate::error::Result;
use crate::fs::{agentfs::ReapHook, BoxedFile, FileSystem};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
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
}

#[derive(Clone)]
struct OpenEntry {
    handle: Handle,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct OpenKey {
    ino: i64,
    authority: Authority,
}

struct HandleTableInner {
    token_capacity: usize,
    open_capacity: usize,
    tokens: HashMap<u64, TokenEntry>,
    token_lru: VecDeque<u64>,
    open_handles: HashMap<OpenKey, OpenEntry>,
    open_lru: VecDeque<OpenKey>,
}

impl HandleTableInner {
    fn new(token_capacity: usize, open_capacity: usize) -> Self {
        Self {
            token_capacity,
            open_capacity,
            tokens: HashMap::new(),
            token_lru: VecDeque::new(),
            open_handles: HashMap::new(),
            open_lru: VecDeque::new(),
        }
    }

    fn touch_token(&mut self, token: u64) {
        if let Some(pos) = self
            .token_lru
            .iter()
            .position(|candidate| *candidate == token)
        {
            self.token_lru.remove(pos);
        }
        self.token_lru.push_back(token);
    }

    fn touch_open(&mut self, key: OpenKey) {
        if let Some(pos) = self.open_lru.iter().position(|candidate| *candidate == key) {
            self.open_lru.remove(pos);
        }
        self.open_lru.push_back(key);
    }

    fn evict_lru_token_if_needed(&mut self) {
        while self.tokens.len() >= self.token_capacity {
            let Some(victim) = self.token_lru.pop_front() else {
                break;
            };
            self.tokens.remove(&victim);
        }
    }

    fn evict_lru_open_if_needed(&mut self) {
        while self.open_handles.len() >= self.open_capacity {
            let Some(victim) = self.open_lru.pop_front() else {
                break;
            };
            self.open_handles.remove(&victim);
        }
    }

    fn invalidate_ino(&mut self, ino: i64) {
        let tokens_to_remove: Vec<u64> = self
            .tokens
            .iter()
            .filter_map(|(token, entry)| (entry.ino == ino).then_some(*token))
            .collect();
        for token in tokens_to_remove {
            self.tokens.remove(&token);
        }
        self.token_lru
            .retain(|token| self.tokens.contains_key(token));

        let open_to_remove: Vec<OpenKey> = self
            .open_handles
            .keys()
            .filter_map(|key| (key.ino == ino).then_some(*key))
            .collect();
        for key in open_to_remove {
            self.open_handles.remove(&key);
        }
        self.open_lru
            .retain(|key| self.open_handles.contains_key(key));
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
        if inner.tokens.contains_key(&token) {
            return false;
        }
        inner.evict_lru_token_if_needed();
        inner.tokens.insert(token, TokenEntry { ino });
        inner.touch_token(token);
        true
    }

    pub fn has_write_authority(&self, ino: i64, token: u64) -> bool {
        let mut inner = self.inner.lock();
        let has_authority = inner
            .tokens
            .get(&token)
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
        let token = inner
            .token_lru
            .iter()
            .rev()
            .find(|token| {
                inner
                    .tokens
                    .get(token)
                    .map(|entry| entry.ino == ino)
                    .unwrap_or(false)
            })
            .copied();
        if let Some(token) = token {
            inner.touch_token(token);
        }
        token
    }

    /// Open and cache one handle per (inode, authority) pair.
    pub async fn open_cached(
        &self,
        fs: &Arc<dyn FileSystem>,
        ino: i64,
        authority: Authority,
    ) -> Result<Handle> {
        let key = OpenKey { ino, authority };
        {
            let mut inner = self.inner.lock();
            if let Some(entry) = inner.open_handles.get(&key).cloned() {
                inner.touch_open(key);
                return Ok(entry.handle);
            }
        }

        let file = fs.open(ino, authority.open_flags()).await?;
        let handle = Handle {
            ino,
            authority,
            file,
        };

        let mut inner = self.inner.lock();
        if let Some(entry) = inner.open_handles.get(&key).cloned() {
            inner.touch_open(key);
            return Ok(entry.handle);
        }
        inner.evict_lru_open_if_needed();
        inner.open_handles.insert(
            key,
            OpenEntry {
                handle: handle.clone(),
            },
        );
        inner.touch_open(key);
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
