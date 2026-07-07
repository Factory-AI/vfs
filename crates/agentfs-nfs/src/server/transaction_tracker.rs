use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

pub const DEFAULT_REPLY_CACHE_CAPACITY: usize = 1024;
pub const DEFAULT_REPLY_CACHE_BYTES: usize = 16 * 1024 * 1024;

/// `TransactionTracker` is a bounded duplicate-reply cache for RPC transactions.
///
/// The cache key is the RPC xid, the client's opaque verifier, the RPC
/// program/version/procedure triple, and a compact digest of the procedure
/// arguments. It deliberately excludes the TCP source port so retransmissions
/// after reconnect can replay the original reply instead of re-executing
/// non-idempotent handlers.
pub struct TransactionTracker {
    completed_capacity: usize,
    completed_byte_capacity: usize,
    transactions: Mutex<TransactionCache>,
}

impl TransactionTracker {
    pub fn new(completed_capacity: usize) -> Self {
        Self::with_limits(completed_capacity, DEFAULT_REPLY_CACHE_BYTES)
    }

    pub fn with_limits(completed_capacity: usize, completed_byte_capacity: usize) -> Self {
        Self {
            completed_capacity,
            completed_byte_capacity,
            transactions: Mutex::new(TransactionCache::default()),
        }
    }

    /// Begins tracking a transaction.
    ///
    /// New transactions return a guard. Completing the guard stores the serialized
    /// reply bytes. Dropping the guard without completion removes the in-progress
    /// entry, which keeps handler panics from blackholing that xid forever.
    pub fn begin(self: &Arc<Self>, key: TransactionKey) -> TransactionLookup {
        let mut cache = self
            .transactions
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        match cache.transactions.get(&key) {
            Some(TransactionState::Completed(reply)) => TransactionLookup::Replay(reply.clone()),
            Some(TransactionState::InProgress) => TransactionLookup::InProgress,
            None => {
                cache
                    .transactions
                    .insert(key.clone(), TransactionState::InProgress);
                TransactionLookup::New(TransactionGuard {
                    tracker: self.clone(),
                    key,
                    completed: false,
                })
            }
        }
    }

    fn complete(&self, key: TransactionKey, reply: Vec<u8>) {
        let mut cache = self
            .transactions
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let reply_len = reply.len();
        if reply_len > self.completed_byte_capacity {
            cache.remove_in_progress(&key);
            return;
        }
        let previous = cache.transactions.insert(
            key.clone(),
            TransactionState::Completed(Arc::from(reply.into_boxed_slice())),
        );
        match previous {
            Some(TransactionState::Completed(previous_reply)) => {
                cache.completed_bytes = cache.completed_bytes.saturating_sub(previous_reply.len());
            }
            _ => {
                cache.completed_count += 1;
            }
        }
        cache.completed_bytes += reply_len;
        cache.completed_order.push_back(key);
        cache.evict_completed(self.completed_capacity, self.completed_byte_capacity);
    }

    fn remove_in_progress(&self, key: &TransactionKey) {
        let mut cache = self
            .transactions
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        cache.remove_in_progress(key);
    }

    #[cfg(test)]
    fn completed_len_for_tests(&self) -> usize {
        self.transactions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .completed_count
    }

    #[cfg(test)]
    fn completed_bytes_for_tests(&self) -> usize {
        self.transactions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .completed_bytes
    }

    #[cfg(test)]
    fn in_progress_len_for_tests(&self) -> usize {
        self.transactions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .transactions
            .values()
            .filter(|state| matches!(state, TransactionState::InProgress))
            .count()
    }
}

pub enum TransactionLookup {
    New(TransactionGuard),
    InProgress,
    Replay(Arc<[u8]>),
}

pub struct TransactionGuard {
    tracker: Arc<TransactionTracker>,
    key: TransactionKey,
    completed: bool,
}

impl TransactionGuard {
    pub fn complete(mut self, reply: Vec<u8>) {
        self.tracker.complete(self.key.clone(), reply);
        self.completed = true;
    }
}

impl Drop for TransactionGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.tracker.remove_in_progress(&self.key);
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TransactionKey {
    xid: u32,
    prog: u32,
    vers: u32,
    proc: u32,
    client_verifier: Vec<u8>,
    args_fingerprint: RequestFingerprint,
}

impl TransactionKey {
    pub(crate) fn new(
        xid: u32,
        prog: u32,
        vers: u32,
        proc: u32,
        client_verifier: Vec<u8>,
        args: &[u8],
    ) -> Self {
        Self {
            xid,
            prog,
            vers,
            proc,
            client_verifier,
            args_fingerprint: RequestFingerprint::from_bytes(args),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RequestFingerprint {
    len: usize,
    hash: u64,
}

impl RequestFingerprint {
    fn from_bytes(bytes: &[u8]) -> Self {
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        Self {
            len: bytes.len(),
            hash: hasher.finish(),
        }
    }
}

#[derive(Default)]
struct TransactionCache {
    transactions: HashMap<TransactionKey, TransactionState>,
    completed_order: VecDeque<TransactionKey>,
    completed_count: usize,
    completed_bytes: usize,
}

impl TransactionCache {
    fn evict_completed(&mut self, completed_capacity: usize, completed_byte_capacity: usize) {
        while self.completed_count > completed_capacity
            || self.completed_bytes > completed_byte_capacity
        {
            let Some(key) = self.completed_order.pop_front() else {
                break;
            };
            if let Some(TransactionState::Completed(reply)) = self.transactions.remove(&key) {
                self.completed_count -= 1;
                self.completed_bytes = self.completed_bytes.saturating_sub(reply.len());
            }
        }
    }

    fn remove_in_progress(&mut self, key: &TransactionKey) {
        if matches!(
            self.transactions.get(key),
            Some(TransactionState::InProgress)
        ) {
            self.transactions.remove(key);
        }
    }
}

enum TransactionState {
    InProgress,
    Completed(Arc<[u8]>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic;
    use std::sync::Arc;

    fn verifier(bytes: &[u8]) -> Vec<u8> {
        bytes.to_vec()
    }

    fn key(xid: u32, verifier_bytes: &[u8], proc: u32, args: &[u8]) -> TransactionKey {
        TransactionKey::new(xid, 100003, 3, proc, verifier(verifier_bytes), args)
    }

    fn complete(tracker: &Arc<TransactionTracker>, xid: u32, verifier_bytes: &[u8], reply: &[u8]) {
        match tracker.begin(key(xid, verifier_bytes, 1, b"args")) {
            TransactionLookup::New(guard) => guard.complete(reply.to_vec()),
            _ => panic!("expected new transaction"),
        }
    }

    #[test]
    fn completed_reply_cache_is_bounded_and_evicts_oldest_completed() {
        let tracker = Arc::new(TransactionTracker::new(2));

        complete(&tracker, 1, b"client", b"reply-1");
        complete(&tracker, 2, b"client", b"reply-2");
        complete(&tracker, 3, b"client", b"reply-3");

        assert_eq!(tracker.completed_len_for_tests(), 2);
        assert!(
            matches!(
                tracker.begin(key(1, b"client", 1, b"args")),
                TransactionLookup::New(_)
            ),
            "oldest completed reply should be evicted when capacity is exceeded"
        );
        assert!(
            matches!(
                tracker.begin(key(2, b"client", 1, b"args")),
                TransactionLookup::Replay(reply) if reply.as_ref() == b"reply-2"
            ),
            "second completed reply should still be cached"
        );
        assert!(
            matches!(
                tracker.begin(key(3, b"client", 1, b"args")),
                TransactionLookup::Replay(reply) if reply.as_ref() == b"reply-3"
            ),
            "newest completed reply should still be cached"
        );
    }

    #[test]
    fn in_progress_guard_drop_cleans_up_after_handler_panic() {
        let tracker = Arc::new(TransactionTracker::new(2));
        let panicking_tracker = tracker.clone();

        let original_hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let result = panic::catch_unwind(move || {
            match panicking_tracker.begin(key(42, b"client", 1, b"args")) {
                TransactionLookup::New(_guard) => panic!("simulated handler panic"),
                _ => panic!("expected new transaction"),
            }
        });
        panic::set_hook(original_hook);
        assert!(result.is_err());

        assert_eq!(tracker.in_progress_len_for_tests(), 0);
        assert!(
            matches!(
                tracker.begin(key(42, b"client", 1, b"args")),
                TransactionLookup::New(_)
            ),
            "dropped in-progress guard should not blackhole the xid forever"
        );
    }

    #[test]
    fn key_includes_rpc_shape_and_argument_digest() {
        let tracker = Arc::new(TransactionTracker::new(8));
        match tracker.begin(key(7, b"client", 8, b"create-a")) {
            TransactionLookup::New(guard) => guard.complete(b"create-a-reply".to_vec()),
            _ => panic!("expected first CREATE to be new"),
        }

        assert!(
            matches!(
                tracker.begin(key(7, b"client", 8, b"create-a")),
                TransactionLookup::Replay(reply) if reply.as_ref() == b"create-a-reply"
            ),
            "identical retransmission should replay"
        );
        assert!(
            matches!(
                tracker.begin(key(7, b"client", 1, b"create-a")),
                TransactionLookup::New(_)
            ),
            "same xid/verifier with a different RPC procedure must not replay"
        );
        assert!(
            matches!(
                tracker.begin(key(7, b"client", 8, b"create-b")),
                TransactionLookup::New(_)
            ),
            "same xid/verifier/procedure with different arguments must not replay"
        );
    }

    #[test]
    fn completed_reply_cache_is_bounded_by_bytes() {
        let tracker = Arc::new(TransactionTracker::with_limits(8, 12));

        complete(&tracker, 1, b"client", b"12345678");
        complete(&tracker, 2, b"client", b"abcdef");

        assert_eq!(tracker.completed_len_for_tests(), 1);
        assert_eq!(tracker.completed_bytes_for_tests(), 6);
        assert!(
            matches!(
                tracker.begin(key(1, b"client", 1, b"args")),
                TransactionLookup::New(_)
            ),
            "oldest reply should be evicted to honor the byte budget"
        );
        assert!(
            matches!(
                tracker.begin(key(2, b"client", 1, b"args")),
                TransactionLookup::Replay(reply) if reply.as_ref() == b"abcdef"
            ),
            "newest reply should remain cached within the byte budget"
        );
    }
}
