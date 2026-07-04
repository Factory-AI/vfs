use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

pub const DEFAULT_REPLY_CACHE_CAPACITY: usize = 1024;

/// `TransactionTracker` is a bounded duplicate-reply cache for RPC transactions.
///
/// The cache key is the RPC xid plus the client's opaque verifier. It deliberately
/// excludes the TCP source port so retransmissions after reconnect can replay the
/// original reply instead of re-executing non-idempotent handlers.
pub struct TransactionTracker {
    completed_capacity: usize,
    transactions: Mutex<TransactionCache>,
}

impl TransactionTracker {
    pub fn new(completed_capacity: usize) -> Self {
        Self {
            completed_capacity,
            transactions: Mutex::new(TransactionCache::default()),
        }
    }

    /// Begins tracking a transaction.
    ///
    /// New transactions return a guard. Completing the guard stores the serialized
    /// reply bytes. Dropping the guard without completion removes the in-progress
    /// entry, which keeps handler panics from blackholing that xid forever.
    pub fn begin(self: &Arc<Self>, xid: u32, client_verifier: Vec<u8>) -> TransactionLookup {
        let key = TransactionKey {
            xid,
            client_verifier,
        };
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
        let previous = cache.transactions.insert(
            key.clone(),
            TransactionState::Completed(Arc::from(reply.into_boxed_slice())),
        );
        if !matches!(previous, Some(TransactionState::Completed(_))) {
            cache.completed_count += 1;
        }
        cache.completed_order.push_back(key);
        cache.evict_completed(self.completed_capacity);
    }

    fn remove_in_progress(&self, key: &TransactionKey) {
        let mut cache = self
            .transactions
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if matches!(
            cache.transactions.get(key),
            Some(TransactionState::InProgress)
        ) {
            cache.transactions.remove(key);
        }
    }

    #[cfg(test)]
    fn completed_len_for_tests(&self) -> usize {
        self.transactions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .completed_count
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
struct TransactionKey {
    xid: u32,
    client_verifier: Vec<u8>,
}

#[derive(Default)]
struct TransactionCache {
    transactions: HashMap<TransactionKey, TransactionState>,
    completed_order: VecDeque<TransactionKey>,
    completed_count: usize,
}

impl TransactionCache {
    fn evict_completed(&mut self, completed_capacity: usize) {
        while self.completed_count > completed_capacity {
            let Some(key) = self.completed_order.pop_front() else {
                break;
            };
            if matches!(
                self.transactions.get(&key),
                Some(TransactionState::Completed(_))
            ) {
                self.transactions.remove(&key);
                self.completed_count -= 1;
            }
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

    fn complete(tracker: &Arc<TransactionTracker>, xid: u32, verifier_bytes: &[u8], reply: &[u8]) {
        match tracker.begin(xid, verifier(verifier_bytes)) {
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
                tracker.begin(1, verifier(b"client")),
                TransactionLookup::New(_)
            ),
            "oldest completed reply should be evicted when capacity is exceeded"
        );
        assert!(
            matches!(
                tracker.begin(2, verifier(b"client")),
                TransactionLookup::Replay(reply) if reply.as_ref() == b"reply-2"
            ),
            "second completed reply should still be cached"
        );
        assert!(
            matches!(
                tracker.begin(3, verifier(b"client")),
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
        let result =
            panic::catch_unwind(
                move || match panicking_tracker.begin(42, verifier(b"client")) {
                    TransactionLookup::New(_guard) => panic!("simulated handler panic"),
                    _ => panic!("expected new transaction"),
                },
            );
        panic::set_hook(original_hook);
        assert!(result.is_err());

        assert_eq!(tracker.in_progress_len_for_tests(), 0);
        assert!(
            matches!(
                tracker.begin(42, verifier(b"client")),
                TransactionLookup::New(_)
            ),
            "dropped in-progress guard should not blackhole the xid forever"
        );
    }
}
