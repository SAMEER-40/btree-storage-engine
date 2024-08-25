use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::lock_manager::{LockManager, LockMode};
use crate::error::{Error, Result};
use crate::wal::wal_manager::WalManager;

/// Transaction states following the standard lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionState {
    /// Transaction is active and can perform operations
    Active,
    /// Transaction is committing (WAL commit record written)
    Committed,
    /// Transaction is aborting
    Aborted,
}

/// Represents a single transaction.
#[derive(Debug)]
pub struct Transaction {
    pub txn_id: u64,
    pub state: TransactionState,
    /// Pages modified by this transaction: page_id -> before_image
    pub write_set: HashMap<u32, Vec<u8>>,
    /// Pages read by this transaction
    pub read_set: Vec<u32>,
}

impl Transaction {
    pub fn new(txn_id: u64) -> Self {
        Transaction {
            txn_id,
            state: TransactionState::Active,
            write_set: HashMap::new(),
            read_set: Vec::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.state == TransactionState::Active
    }
}

/// Transaction Manager — coordinates ACID transactions.
///
/// Provides:
/// - **Atomicity**: via WAL undo on abort
/// - **Consistency**: via lock manager preventing conflicting access
/// - **Isolation**: via Strict 2PL (locks held until commit/abort)
/// - **Durability**: via WAL force on commit
pub struct TransactionManager {
    next_txn_id: AtomicU64,
    /// Active transactions
    active_txns: HashMap<u64, Transaction>,
    /// Lock manager for concurrency control
    lock_manager: LockManager,
}

impl TransactionManager {
    pub fn new() -> Self {
        TransactionManager {
            next_txn_id: AtomicU64::new(1),
            active_txns: HashMap::new(),
            lock_manager: LockManager::new(),
        }
    }

    /// Begin a new transaction.
    pub fn begin(&mut self, wal: &mut WalManager) -> Result<u64> {
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::SeqCst);
        let txn = Transaction::new(txn_id);
        self.active_txns.insert(txn_id, txn);

        // Write BEGIN record to WAL
        wal.log_begin(txn_id)?;

        Ok(txn_id)
    }

    /// Acquire a read lock on a page for a transaction.
    pub fn acquire_read_lock(&mut self, txn_id: u64, page_id: u32) -> Result<()> {
        if !self.active_txns.contains_key(&txn_id) {
            return Err(Error::TransactionAborted(txn_id));
        }

        self.lock_manager
            .acquire(txn_id, page_id, LockMode::Shared)?;

        if let Some(txn) = self.active_txns.get_mut(&txn_id) {
            txn.read_set.push(page_id);
        }

        Ok(())
    }

    /// Acquire a write lock on a page for a transaction, recording the before-image.
    pub fn acquire_write_lock(
        &mut self,
        txn_id: u64,
        page_id: u32,
        before_image: &[u8],
    ) -> Result<()> {
        if !self.active_txns.contains_key(&txn_id) {
            return Err(Error::TransactionAborted(txn_id));
        }

        self.lock_manager
            .acquire(txn_id, page_id, LockMode::Exclusive)?;

        if let Some(txn) = self.active_txns.get_mut(&txn_id) {
            // Only store the first before-image (original state)
            txn.write_set
                .entry(page_id)
                .or_insert_with(|| before_image.to_vec());
        }

        Ok(())
    }

    /// Record a page write to WAL (called after modifying the page in the buffer pool).
    pub fn log_write(
        &mut self,
        wal: &mut WalManager,
        txn_id: u64,
        page_id: u32,
        before_image: &[u8],
        after_image: &[u8],
    ) -> Result<()> {
        if !self.active_txns.contains_key(&txn_id) {
            return Err(Error::TransactionAborted(txn_id));
        }

        wal.log_page_write(txn_id, page_id, before_image, after_image)?;
        Ok(())
    }

    /// Commit a transaction.
    ///
    /// 1. Write COMMIT record to WAL
    /// 2. Force WAL flush (ensures durability)
    /// 3. Release all locks
    pub fn commit(&mut self, wal: &mut WalManager, txn_id: u64) -> Result<()> {
        if let Some(txn) = self.active_txns.get_mut(&txn_id) {
            if txn.state != TransactionState::Active {
                return Err(Error::TransactionAborted(txn_id));
            }
            txn.state = TransactionState::Committed;
        } else {
            return Err(Error::TransactionAborted(txn_id));
        }

        // Write COMMIT to WAL and flush (force)
        wal.log_commit(txn_id)?;

        // Release all locks (S2PL: locks held until commit)
        self.lock_manager.release_all(txn_id);

        // Remove from active transactions
        self.active_txns.remove(&txn_id);

        Ok(())
    }

    /// Abort a transaction.
    ///
    /// 1. Write ABORT record to WAL
    /// 2. Return the write set for undo by the caller
    /// 3. Release all locks
    pub fn abort(&mut self, wal: &mut WalManager, txn_id: u64) -> Result<HashMap<u32, Vec<u8>>> {
        let write_set = if let Some(txn) = self.active_txns.get_mut(&txn_id) {
            txn.state = TransactionState::Aborted;
            txn.write_set.clone()
        } else {
            return Err(Error::TransactionAborted(txn_id));
        };

        // Write ABORT to WAL
        wal.log_abort(txn_id)?;

        // Release all locks
        self.lock_manager.release_all(txn_id);

        // Remove from active transactions
        self.active_txns.remove(&txn_id);

        // Return write set so the caller can restore before-images
        Ok(write_set)
    }

    /// Get the IDs of all active transactions (for checkpointing)
    pub fn active_transaction_ids(&self) -> Vec<u64> {
        self.active_txns.keys().copied().collect()
    }

    /// Number of active transactions
    pub fn active_count(&self) -> usize {
        self.active_txns.len()
    }

    /// Check if a transaction is active
    pub fn is_active(&self, txn_id: u64) -> bool {
        self.active_txns.contains_key(&txn_id)
    }

    /// Get a reference to the lock manager
    pub fn lock_manager(&self) -> &LockManager {
        &self.lock_manager
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let id = COUNTER.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!("storagedb_txn_test_{}", id));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn create_wal() -> (WalManager, TempDir) {
        let dir = TempDir::new();
        let path = dir.path().join("test.wal");
        (WalManager::open(&path).unwrap(), dir)
    }

    #[test]
    fn test_begin_and_commit() {
        let (mut wal, _dir) = create_wal();
        let mut tm = TransactionManager::new();

        let txn_id = tm.begin(&mut wal).unwrap();
        assert!(tm.is_active(txn_id));
        assert_eq!(tm.active_count(), 1);

        tm.commit(&mut wal, txn_id).unwrap();
        assert!(!tm.is_active(txn_id));
        assert_eq!(tm.active_count(), 0);
    }

    #[test]
    fn test_begin_and_abort() {
        let (mut wal, _dir) = create_wal();
        let mut tm = TransactionManager::new();

        let txn_id = tm.begin(&mut wal).unwrap();
        tm.acquire_write_lock(txn_id, 10, b"before").unwrap();

        let write_set = tm.abort(&mut wal, txn_id).unwrap();
        assert!(write_set.contains_key(&10));
        assert_eq!(write_set[&10], b"before".to_vec());
        assert!(!tm.is_active(txn_id));
    }

    #[test]
    fn test_lock_conflict_between_transactions() {
        let (mut wal, _dir) = create_wal();
        let mut tm = TransactionManager::new();

        let txn1 = tm.begin(&mut wal).unwrap();
        let txn2 = tm.begin(&mut wal).unwrap();

        // Txn1 acquires exclusive lock on page 10
        tm.acquire_write_lock(txn1, 10, b"before").unwrap();

        // Txn2 tries to read page 10 — should conflict
        let result = tm.acquire_read_lock(txn2, 10);
        assert!(result.is_err());
    }

    #[test]
    fn test_shared_read_locks_compatible() {
        let (mut wal, _dir) = create_wal();
        let mut tm = TransactionManager::new();

        let txn1 = tm.begin(&mut wal).unwrap();
        let txn2 = tm.begin(&mut wal).unwrap();

        tm.acquire_read_lock(txn1, 10).unwrap();
        tm.acquire_read_lock(txn2, 10).unwrap(); // should succeed
    }

    #[test]
    fn test_wal_records_written() {
        let (mut wal, _dir) = create_wal();
        let mut tm = TransactionManager::new();

        let txn_id = tm.begin(&mut wal).unwrap();
        tm.log_write(&mut wal, txn_id, 42, b"old", b"new").unwrap();
        tm.commit(&mut wal, txn_id).unwrap();

        let records = wal.read_all_records().unwrap();
        assert_eq!(records.len(), 3); // BEGIN, PAGE_WRITE, COMMIT
    }

    #[test]
    fn test_multiple_concurrent_transactions() {
        let (mut wal, _dir) = create_wal();
        let mut tm = TransactionManager::new();

        let txn1 = tm.begin(&mut wal).unwrap();
        let txn2 = tm.begin(&mut wal).unwrap();
        let txn3 = tm.begin(&mut wal).unwrap();

        // Different pages — no conflicts
        tm.acquire_write_lock(txn1, 10, b"b1").unwrap();
        tm.acquire_write_lock(txn2, 20, b"b2").unwrap();
        tm.acquire_write_lock(txn3, 30, b"b3").unwrap();

        assert_eq!(tm.active_count(), 3);

        tm.commit(&mut wal, txn1).unwrap();
        tm.commit(&mut wal, txn2).unwrap();
        tm.abort(&mut wal, txn3).unwrap();

        assert_eq!(tm.active_count(), 0);
    }
}
