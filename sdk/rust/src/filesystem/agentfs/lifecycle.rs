use crate::connection_pool::ConnectionPool;
use crate::error::Result;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::Connection;

/// Hook invoked in the same SQLite transaction that reaps an inode.
///
/// Implementors may delete sidecar rows keyed by `ino`; returning an error
/// aborts and rolls back the whole reap transaction, including core row
/// deletion.
#[async_trait]
pub trait ReapHook: Send + Sync {
    fn dedup_key(&self) -> Option<&'static str> {
        None
    }

    async fn on_reap(&self, conn: &Connection, ino: i64) -> Result<()>;
}

/// Owns open-inode tracking, deferred orphan queues, and reap hooks.
#[derive(Default)]
pub(crate) struct Lifecycle {
    open_inodes: Arc<OpenInodes>,
    reap_hooks: Mutex<Vec<Arc<dyn ReapHook>>>,
}

impl Lifecycle {
    pub(crate) fn guard(&self, ino: i64) -> OpenInodeGuard {
        self.open_inodes.guard(ino)
    }

    /// Marks the inode for deferred reaping when handles are live.
    /// Returns true when the caller must NOT delete the rows yet.
    pub(crate) fn defer_reap_if_open(&self, ino: i64) -> bool {
        self.open_inodes.defer_reap_if_open(ino)
    }

    pub fn register_reap_hook(&self, hook: Arc<dyn ReapHook>) -> bool {
        let mut hooks = self.reap_hooks.lock().unwrap();
        if let Some(key) = hook.dedup_key() {
            if hooks
                .iter()
                .any(|registered| registered.dedup_key() == Some(key))
            {
                return false;
            }
        }
        hooks.push(hook);
        true
    }

    /// Sweep POSIX orphans a crash stranded: nlink = 0 rows are files that
    /// were unlinked while open and never queued for deferred reap because the
    /// process died. They are invisible (no dentry), so deleting them before
    /// serving is safe.
    pub(crate) async fn sweep_mount_orphans(&self, conn: &Connection) -> Result<Vec<i64>> {
        let inos = self.nlink_zero_inos(conn).await?;
        if inos.is_empty() {
            return Ok(Vec::new());
        }

        let txn = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?;
        let result: Result<Vec<i64>> = async {
            let mut reaped = Vec::new();
            for ino in &inos {
                if self.reap_inode_with_conn(conn, *ino).await? {
                    reaped.push(*ino);
                }
            }
            Ok(reaped)
        }
        .await;

        match result {
            Ok(reaped) => {
                txn.commit().await?;
                Ok(reaped)
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    /// Reap inodes whose deletion unlink/rename deferred because open
    /// handles existed (POSIX unlink-while-open). Runs opportunistically at
    /// namespace mutations and at finalize; crash recovery is covered by the
    /// mount sweep.
    pub(crate) async fn process_deferred_reaps<F>(
        &self,
        pool: &ConnectionPool,
        before_reap: F,
    ) -> Result<Vec<i64>>
    where
        F: Fn(i64),
    {
        if !self.open_inodes.has_pending_reaps() {
            return Ok(Vec::new());
        }
        let inos = self.open_inodes.take_reap_queue();
        for ino in &inos {
            before_reap(*ino);
        }
        let conn = pool.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<Vec<i64>> = async {
            let mut reaped = Vec::new();
            for ino in &inos {
                if self.reap_inode_with_conn(&conn, *ino).await? {
                    reaped.push(*ino);
                }
            }
            Ok(reaped)
        }
        .await;

        match result {
            Ok(reaped) => {
                txn.commit().await?;
                Ok(reaped)
            }
            Err(error) => {
                let _ = txn.rollback().await;
                self.open_inodes.requeue_reaps(inos);
                Err(error)
            }
        }
    }

    /// Delete an already-unlinked inode and its core storage rows using the
    /// caller's transaction. The nlink=0 guard makes stale queue entries a
    /// no-op.
    pub(crate) async fn reap_inode_with_conn(&self, conn: &Connection, ino: i64) -> Result<bool> {
        let changed = conn
            .execute("DELETE FROM fs_inode WHERE ino = ? AND nlink = 0", (ino,))
            .await?;
        if changed == 0 {
            return Ok(false);
        }

        for hook in self.hooks_snapshot() {
            hook.on_reap(conn, ino).await?;
        }
        conn.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))
            .await?;
        conn.execute("DELETE FROM fs_symlink WHERE ino = ?", (ino,))
            .await?;
        Ok(true)
    }

    fn hooks_snapshot(&self) -> Vec<Arc<dyn ReapHook>> {
        self.reap_hooks.lock().unwrap().iter().cloned().collect()
    }

    #[cfg(test)]
    pub(crate) fn reap_hook_count(&self) -> usize {
        self.reap_hooks.lock().unwrap().len()
    }

    async fn nlink_zero_inos(&self, conn: &Connection) -> Result<Vec<i64>> {
        let mut rows = conn
            .query("SELECT ino FROM fs_inode WHERE nlink = 0", ())
            .await?;
        let mut inos = Vec::new();
        while let Some(row) = rows.next().await? {
            let ino: i64 = row.get(0)?;
            inos.push(ino);
        }
        Ok(inos)
    }
}

/// Tracks inodes with live `AgentFSFile` handles so unlink and
/// rename-replace can defer row deletion: POSIX requires an
/// unlinked-but-open file to stay readable and writable until its last
/// handle closes.
#[derive(Default)]
struct OpenInodes {
    inner: Mutex<OpenInodesInner>,
}

#[derive(Default)]
struct OpenInodesInner {
    counts: HashMap<i64, u32>,
    orphaned: HashSet<i64>,
    reap_queue: Vec<i64>,
}

impl OpenInodes {
    fn guard(self: &Arc<Self>, ino: i64) -> OpenInodeGuard {
        let mut inner = self.inner.lock().unwrap();
        *inner.counts.entry(ino).or_insert(0) += 1;
        OpenInodeGuard {
            registry: Arc::clone(self),
            ino,
        }
    }

    fn defer_reap_if_open(&self, ino: i64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.counts.contains_key(&ino) {
            inner.orphaned.insert(ino);
            true
        } else {
            false
        }
    }

    fn release(&self, ino: i64) {
        let mut inner = self.inner.lock().unwrap();
        match inner.counts.get_mut(&ino) {
            Some(count) if *count > 1 => *count -= 1,
            Some(_) => {
                inner.counts.remove(&ino);
                if inner.orphaned.remove(&ino) {
                    inner.reap_queue.push(ino);
                }
            }
            None => {}
        }
    }

    fn take_reap_queue(&self) -> Vec<i64> {
        let mut inner = self.inner.lock().unwrap();
        std::mem::take(&mut inner.reap_queue)
    }

    fn requeue_reaps(&self, inos: Vec<i64>) {
        let mut inner = self.inner.lock().unwrap();
        inner.reap_queue.extend(inos);
    }

    fn has_pending_reaps(&self) -> bool {
        !self.inner.lock().unwrap().reap_queue.is_empty()
    }
}

/// RAII registration of one `AgentFSFile` in [`OpenInodes`].
pub(crate) struct OpenInodeGuard {
    registry: Arc<OpenInodes>,
    ino: i64,
}

impl Drop for OpenInodeGuard {
    fn drop(&mut self) {
        self.registry.release(self.ino);
    }
}
