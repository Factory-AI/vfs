//! Connection pool for Turso database connections.
//!
//! This module provides a thread-safe connection pool that manages database
//! connections with a maximum limit. When the pool is exhausted, callers block
//! until a connection becomes available or timeout occurs.

use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use turso::{Connection, Database};

use crate::error::{Error, Result};

/// Default number of connections in a local file-backed pool.
const DEFAULT_MAX_CONNECTIONS: usize = 8;

/// Default timeout for acquiring a connection from the pool.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Configuration for a connection pool.
#[derive(Clone, Debug)]
pub struct PoolOptions {
    /// Maximum number of connections that may be checked out concurrently.
    pub max_connections: usize,
    /// Timeout for acquiring a connection when the pool is exhausted.
    pub timeout: Duration,
    /// SQL statements applied once to every newly-created connection.
    pub setup_sql: Vec<String>,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            timeout: DEFAULT_TIMEOUT,
            setup_sql: Vec::new(),
        }
    }
}

impl PoolOptions {
    /// Options for a strictly serialized single-connection pool.
    pub fn single_connection() -> Self {
        Self {
            max_connections: 1,
            ..Self::default()
        }
    }

    /// Override the acquisition timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the setup SQL applied to every newly-created connection.
    pub fn with_setup_sql<I, S>(mut self, setup_sql: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.setup_sql = setup_sql.into_iter().map(Into::into).collect();
        self
    }
}

/// Database wrapper that supports both regular and sync databases.
pub enum DatabaseType {
    /// A local Turso database.
    Local(Database),
    /// A Turso sync database.
    Sync(turso::sync::Database),
}

/// A pool of database connections with a maximum limit.
///
/// The pool enforces a maximum number of concurrent connections. When all
/// connections are in use, `get_connection()` blocks until one becomes
/// available or the timeout expires (returning `ConnectionPoolTimeout`).
#[derive(Clone)]
pub struct ConnectionPool {
    inner: Arc<ConnectionPoolInner>,
}

struct ConnectionPoolInner {
    db: DatabaseType,
    /// Available connections ready to be reused
    pool: Mutex<Vec<Connection>>,
    /// Semaphore to limit concurrent connections
    semaphore: Arc<Semaphore>,
    /// Timeout for acquiring a connection
    timeout: Duration,
    /// SQL statements applied once to each newly-created connection
    setup_sql: Vec<String>,
}

impl ConnectionPool {
    /// Create a connection pool with explicit database type and options.
    pub fn with_options(db: DatabaseType, options: PoolOptions) -> Self {
        Self {
            inner: Arc::new(ConnectionPoolInner {
                db,
                pool: Mutex::new(Vec::new()),
                semaphore: Arc::new(Semaphore::new(options.max_connections.max(1))),
                timeout: options.timeout,
                setup_sql: options.setup_sql,
            }),
        }
    }

    /// Get a connection from the pool.
    ///
    /// If a pooled connection is available, it is returned immediately.
    /// Otherwise, if the pool hasn't reached max capacity, a new connection
    /// is created. If at max capacity, this blocks until a connection is
    /// returned to the pool or timeout expires.
    ///
    /// # Errors
    ///
    /// Returns `Error::ConnectionPoolTimeout` if no connection becomes
    /// available within the timeout period.
    pub async fn get_connection(&self) -> Result<PooledConnection> {
        // Try to acquire a permit with timeout
        let permit = {
            let _wait_timer =
                crate::telemetry::timer(&crate::telemetry::CORE_COUNTERS.connection_wait);
            tokio::time::timeout(
                self.inner.timeout,
                Arc::clone(&self.inner.semaphore).acquire_owned(),
            )
            .await
            .map_err(|_| Error::ConnectionPoolTimeout)?
            .map_err(|_| Error::Internal("semaphore closed".to_string()))?
        };

        // We have a permit - try to get an existing connection or create new one
        let conn = {
            let mut pool = self.inner.pool.lock().await;
            pool.pop()
        };

        let conn = match conn {
            Some(c) => {
                crate::telemetry::record_connection_reuse();
                c
            }
            None => {
                let conn = self.create_connection().await?;
                crate::telemetry::record_connection_create();
                conn
            }
        };

        Ok(PooledConnection {
            conn: Some(conn),
            pool: self.inner.clone(),
            discard_on_drop: false,
            _permit: permit,
        })
    }

    /// Get the underlying database reference (for creating additional connections).
    /// Returns None if this is a sync database.
    pub fn database(&self) -> Option<&Database> {
        match &self.inner.db {
            DatabaseType::Local(db) => Some(db),
            DatabaseType::Sync(_) => None,
        }
    }

    /// Get the underlying sync database reference.
    pub fn sync_database(&self) -> Option<&turso::sync::Database> {
        match &self.inner.db {
            DatabaseType::Local(_) => None,
            DatabaseType::Sync(db) => Some(db),
        }
    }

    async fn create_connection(&self) -> Result<Connection> {
        let conn = match &self.inner.db {
            DatabaseType::Local(db) => db.connect()?,
            DatabaseType::Sync(db) => db.connect().await?,
        };

        for sql in &self.inner.setup_sql {
            let mut rows = conn.query(sql.as_str(), ()).await?;
            while rows.next().await?.is_some() {}
        }

        Ok(conn)
    }
}

/// A connection borrowed from the pool.
///
/// When dropped, the connection is returned to the pool for reuse and the
/// semaphore permit is released, allowing another caller to acquire a connection.
pub struct PooledConnection {
    conn: Option<Connection>,
    pool: Arc<ConnectionPoolInner>,
    discard_on_drop: bool,
    /// Held permit - released when this is dropped
    _permit: OwnedSemaphorePermit,
}

impl PooledConnection {
    /// Get a reference to the underlying connection.
    pub fn connection(&self) -> &Connection {
        self.conn.as_ref().expect("connection already taken")
    }

    /// Mark this connection as unhealthy so it is evicted instead of reused.
    ///
    /// Callers that observe a fatal database error can keep the pool from
    /// handing the same connection to the next borrower. The semaphore permit
    /// is still released normally when the pooled wrapper drops.
    pub fn mark_unhealthy(&mut self) {
        self.discard_on_drop = true;
    }
}

impl std::ops::Deref for PooledConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection()
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if self.discard_on_drop {
                crate::telemetry::record_connection_health_eviction();
                return;
            }
            // Return connection to pool - use try_lock to avoid blocking in drop
            // If we can't get the lock, just drop the connection (it will be recreated)
            if let Ok(mut pool) = self.pool.pool.try_lock() {
                pool.push(conn);
            } else {
                crate::telemetry::record_connection_drop_discard();
            }
            // Permit is automatically released when _permit is dropped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use turso::Builder;

    #[tokio::test]
    async fn test_connection_pool_basic() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool =
            ConnectionPool::with_options(DatabaseType::Local(db), PoolOptions::single_connection());

        // Get a connection
        let conn = pool.get_connection().await.unwrap();
        assert!(conn.conn.is_some());

        // Drop it
        drop(conn);

        // Get another - should reuse the pooled one
        let conn2 = pool.get_connection().await.unwrap();
        assert!(conn2.conn.is_some());
    }

    #[tokio::test]
    async fn test_default_pool_is_single_connection() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool =
            ConnectionPool::with_options(DatabaseType::Local(db), PoolOptions::single_connection());

        let conn1 = pool.get_connection().await.unwrap();
        let pool_clone = pool.clone();
        let result =
            tokio::time::timeout(Duration::from_millis(100), pool_clone.get_connection()).await;

        assert!(result.is_err());
        drop(conn1);
        assert!(pool.get_connection().await.is_ok());
    }

    #[tokio::test]
    async fn test_single_connection_pool_times_out_under_contention() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool = ConnectionPool::with_options(
            DatabaseType::Local(db),
            PoolOptions::single_connection().with_timeout(Duration::from_millis(50)),
        );

        // Get the one allowed connection
        let conn1 = pool.get_connection().await.unwrap();
        assert!(conn1.conn.is_some());

        // Try to get another - should timeout quickly
        let result = pool.get_connection().await;
        assert!(matches!(result, Err(Error::ConnectionPoolTimeout)));

        // Drop conn1, now we should be able to get a connection
        drop(conn1);
        let conn2 = pool.get_connection().await.unwrap();
        assert!(conn2.conn.is_some());
    }

    #[tokio::test]
    async fn test_connection_pool_timeout_error() {
        // Create pool with very short timeout
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool = ConnectionPool::with_options(
            DatabaseType::Local(db),
            PoolOptions::single_connection().with_timeout(Duration::from_millis(50)),
        );

        // Hold the one connection
        let _conn1 = pool.get_connection().await.unwrap();

        // Try to get another - should return ConnectionPoolTimeout
        let result = pool.get_connection().await;
        assert!(matches!(result, Err(Error::ConnectionPoolTimeout)));
    }

    #[tokio::test]
    async fn test_connection_pool_concurrent_waiters() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool =
            ConnectionPool::with_options(DatabaseType::Local(db), PoolOptions::single_connection());
        let counter = Arc::new(AtomicUsize::new(0));

        // Spawn multiple tasks that all want the connection
        let mut handles = vec![];
        for _ in 0..5 {
            let pool = pool.clone();
            let counter = counter.clone();
            handles.push(tokio::spawn(async move {
                let _conn = pool.get_connection().await.unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
                // Hold connection briefly
                tokio::time::sleep(Duration::from_millis(10)).await;
            }));
        }

        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap();
        }

        // All 5 should have completed (serially, since max=1)
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn test_drop_discard_is_counted_when_pool_lock_is_busy() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool =
            ConnectionPool::with_options(DatabaseType::Local(db), PoolOptions::single_connection());

        let conn = pool.get_connection().await.unwrap();
        let before = crate::telemetry::snapshot().counter("connection_drop_discards");
        let guard = pool.inner.pool.lock().await;
        drop(conn);
        drop(guard);

        let after = crate::telemetry::snapshot().counter("connection_drop_discards");
        assert_eq!(after, before + 1);
    }

    #[tokio::test]
    async fn test_unhealthy_connection_is_evicted_and_counted() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool =
            ConnectionPool::with_options(DatabaseType::Local(db), PoolOptions::single_connection());

        let mut conn = pool.get_connection().await.unwrap();
        let before = crate::telemetry::snapshot().counter("connection_health_evictions");
        conn.mark_unhealthy();
        drop(conn);

        let after = crate::telemetry::snapshot().counter("connection_health_evictions");
        assert_eq!(after, before + 1);
        assert_eq!(pool.inner.pool.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn test_file_backed_pool_allows_multiple_connections() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("pool.db");
        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let pool = ConnectionPool::with_options(
            DatabaseType::Local(db),
            PoolOptions {
                max_connections: 2,
                ..PoolOptions::default()
            },
        );

        let conn1 = pool.get_connection().await.unwrap();
        conn1
            .execute(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT)",
                (),
            )
            .await
            .unwrap();
        conn1
            .execute("INSERT INTO items (value) VALUES ('ok')", ())
            .await
            .unwrap();

        let conn2 = pool.get_connection().await.unwrap();
        let mut rows = conn2
            .query("SELECT value FROM items WHERE id = 1", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<String>(0).unwrap(), "ok");
    }

    #[tokio::test]
    async fn test_setup_sql_runs_on_each_new_connection() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("setup.db");
        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let pool = ConnectionPool::with_options(
            DatabaseType::Local(db),
            PoolOptions {
                max_connections: 2,
                ..PoolOptions::default().with_setup_sql(["PRAGMA busy_timeout = 1234"])
            },
        );

        let conn1 = pool.get_connection().await.unwrap();
        let conn2 = pool.get_connection().await.unwrap();

        for conn in [&conn1, &conn2] {
            let mut rows = conn.query("PRAGMA busy_timeout", ()).await.unwrap();
            let row = rows.next().await.unwrap().unwrap();
            assert_eq!(row.get::<i64>(0).unwrap(), 1234);
        }
    }
}
