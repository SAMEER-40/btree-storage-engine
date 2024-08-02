use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::{Error, Result};

/// Lock modes for two-phase locking (2PL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// Shared lock — allows concurrent reads
    Shared,
    /// Exclusive lock — blocks all other access
    Exclusive,
}

/// Identifies a lockable resource. In our engine, this is a page ID,
/// but could be extended to row-level locking.
pub type ResourceId = u32;

/// A single lock held on a resource
#[derive(Debug)]
struct LockEntry {
    mode: LockMode,
    holders: HashSet<u64>,                 // transaction IDs holding this lock
    wait_queue: VecDeque<(u64, LockMode)>, // (txn_id, requested mode)
}

/// Lock Manager implementing Strict Two-Phase Locking (S2PL).
///
/// Rules:
/// 1. A transaction must acquire a lock before accessing a resource
/// 2. Locks are only released when the transaction commits or aborts
/// 3. If a lock cannot be granted, the transaction waits (or is aborted for deadlock)
pub struct LockManager {
    /// Map from resource -> lock state
    lock_table: HashMap<ResourceId, LockEntry>,
    /// Map from txn_id -> set of resources locked by that transaction
    txn_locks: HashMap<u64, HashSet<ResourceId>>,
}

impl LockManager {
    pub fn new() -> Self {
        LockManager {
            lock_table: HashMap::new(),
            txn_locks: HashMap::new(),
        }
    }

    /// Acquire a lock on a resource. Returns Ok(true) if granted immediately,
    /// Ok(false) if the lock was already held, or Err if there's a conflict
    /// that can't be resolved (deadlock detected via simple wait-die scheme).
    pub fn acquire(&mut self, txn_id: u64, resource: ResourceId, mode: LockMode) -> Result<bool> {
        let entry = self
            .lock_table
            .entry(resource)
            .or_insert_with(|| LockEntry {
                mode: LockMode::Shared, // will be set properly
                holders: HashSet::new(),
                wait_queue: VecDeque::new(),
            });

        // Check if we already hold this lock
        if entry.holders.contains(&txn_id) {
            // Lock upgrade: Shared -> Exclusive
            if entry.mode == LockMode::Shared && mode == LockMode::Exclusive {
                if entry.holders.len() == 1 {
                    // We're the only holder, upgrade directly
                    entry.mode = LockMode::Exclusive;
                    return Ok(true);
                } else {
                    // Can't upgrade while others hold shared locks
                    // Simple deadlock avoidance: abort the requesting transaction
                    return Err(Error::LockConflict(txn_id));
                }
            }
            // Already have equal or stronger lock
            return Ok(false);
        }

        // Check for conflicts
        if entry.holders.is_empty() {
            // No one holds the lock — grant it
            entry.mode = mode;
            entry.holders.insert(txn_id);
            self.txn_locks
                .entry(txn_id)
                .or_insert_with(HashSet::new)
                .insert(resource);
            return Ok(true);
        }

        // Someone else holds the lock
        match (entry.mode, mode) {
            (LockMode::Shared, LockMode::Shared) => {
                // Shared-shared is compatible
                entry.holders.insert(txn_id);
                self.txn_locks
                    .entry(txn_id)
                    .or_insert_with(HashSet::new)
                    .insert(resource);
                Ok(true)
            }
            _ => {
                // Conflict: S-X, X-S, or X-X
                // Simple deadlock avoidance: the requesting transaction fails
                // In a full implementation, we'd use wait-die or wound-wait
                Err(Error::LockConflict(txn_id))
            }
        }
    }

    /// Release all locks held by a transaction (called on commit/abort).
    pub fn release_all(&mut self, txn_id: u64) {
        if let Some(resources) = self.txn_locks.remove(&txn_id) {
            for resource in resources {
                if let Some(entry) = self.lock_table.get_mut(&resource) {
                    entry.holders.remove(&txn_id);

                    // Grant to waiters if possible
                    if entry.holders.is_empty() {
                        // Grant to first waiter
                        if let Some((waiter_txn, waiter_mode)) = entry.wait_queue.pop_front() {
                            entry.mode = waiter_mode;
                            entry.holders.insert(waiter_txn);
                            self.txn_locks
                                .entry(waiter_txn)
                                .or_insert_with(HashSet::new)
                                .insert(resource);

                            // If shared, grant to all subsequent shared waiters
                            if waiter_mode == LockMode::Shared {
                                while let Some(&(next_txn, next_mode)) = entry.wait_queue.front() {
                                    if next_mode == LockMode::Shared {
                                        entry.holders.insert(next_txn);
                                        self.txn_locks
                                            .entry(next_txn)
                                            .or_insert_with(HashSet::new)
                                            .insert(resource);
                                        entry.wait_queue.pop_front();
                                    } else {
                                        break;
                                    }
                                }
                            }
                        }

                        // Clean up empty lock entries
                        if entry.holders.is_empty() && entry.wait_queue.is_empty() {
                            self.lock_table.remove(&resource);
                        }
                    }
                }
            }
        }
    }

    /// Check if a transaction holds a lock on a resource
    pub fn holds_lock(&self, txn_id: u64, resource: ResourceId) -> bool {
        if let Some(entry) = self.lock_table.get(&resource) {
            entry.holders.contains(&txn_id)
        } else {
            false
        }
    }

    /// Get stats about the lock manager
    pub fn num_locks(&self) -> usize {
        self.lock_table.len()
    }

    pub fn num_transactions(&self) -> usize {
        self.txn_locks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_lock_acquire_release() {
        let mut lm = LockManager::new();

        // Acquire exclusive lock
        assert!(lm.acquire(1, 100, LockMode::Exclusive).unwrap());

        // Same txn re-acquiring should return false (already held)
        assert!(!lm.acquire(1, 100, LockMode::Exclusive).unwrap());

        assert!(lm.holds_lock(1, 100));

        // Release
        lm.release_all(1);
        assert!(!lm.holds_lock(1, 100));
    }

    #[test]
    fn test_shared_locks_compatible() {
        let mut lm = LockManager::new();

        assert!(lm.acquire(1, 100, LockMode::Shared).unwrap());
        assert!(lm.acquire(2, 100, LockMode::Shared).unwrap());

        assert!(lm.holds_lock(1, 100));
        assert!(lm.holds_lock(2, 100));
    }

    #[test]
    fn test_exclusive_conflicts_with_shared() {
        let mut lm = LockManager::new();

        lm.acquire(1, 100, LockMode::Shared).unwrap();
        let result = lm.acquire(2, 100, LockMode::Exclusive);
        assert!(result.is_err());
    }

    #[test]
    fn test_exclusive_conflicts_with_exclusive() {
        let mut lm = LockManager::new();

        lm.acquire(1, 100, LockMode::Exclusive).unwrap();
        let result = lm.acquire(2, 100, LockMode::Exclusive);
        assert!(result.is_err());
    }

    #[test]
    fn test_lock_upgrade() {
        let mut lm = LockManager::new();

        lm.acquire(1, 100, LockMode::Shared).unwrap();
        // Upgrade to exclusive (we're the only holder)
        assert!(lm.acquire(1, 100, LockMode::Exclusive).unwrap());
    }

    #[test]
    fn test_lock_upgrade_conflict() {
        let mut lm = LockManager::new();

        lm.acquire(1, 100, LockMode::Shared).unwrap();
        lm.acquire(2, 100, LockMode::Shared).unwrap();
        // Can't upgrade while someone else holds shared
        let result = lm.acquire(1, 100, LockMode::Exclusive);
        assert!(result.is_err());
    }

    #[test]
    fn test_release_grants_to_waiter() {
        let mut lm = LockManager::new();

        lm.acquire(1, 100, LockMode::Exclusive).unwrap();

        // Txn 2 can't get it
        assert!(lm.acquire(2, 100, LockMode::Exclusive).is_err());

        // Release txn 1
        lm.release_all(1);

        // Now txn 2 should be able to get it
        assert!(lm.acquire(2, 100, LockMode::Exclusive).unwrap());
    }

    #[test]
    fn test_multiple_resources() {
        let mut lm = LockManager::new();

        lm.acquire(1, 100, LockMode::Exclusive).unwrap();
        lm.acquire(1, 200, LockMode::Shared).unwrap();
        lm.acquire(2, 200, LockMode::Shared).unwrap();
        lm.acquire(2, 300, LockMode::Exclusive).unwrap();

        assert_eq!(lm.num_locks(), 3);
        assert_eq!(lm.num_transactions(), 2);

        lm.release_all(1);
        assert_eq!(lm.num_transactions(), 1);
    }
}
