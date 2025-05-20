use std::path::{Path, PathBuf};

use crate::btree::BPlusTree;
use crate::buffer::BufferPoolManager;
use crate::disk::disk_manager::DiskManager;
use crate::disk::page::PAGE_SIZE;
use crate::error::Result;
use crate::txn::txn_manager::TransactionManager;
use crate::wal::wal_manager::WalManager;

/// Default buffer pool capacity (number of pages cached in memory).
const DEFAULT_POOL_CAPACITY: usize = 1024;

/// The main storage engine — a unified interface to the database.
///
/// Provides a key-value store backed by:
/// - B+ tree for indexing
/// - Buffer pool with CLOCK eviction for page caching
/// - WAL for crash recovery
/// - Transaction manager for ACID semantics
///
/// Usage:
/// ```no_run
/// use storagedb::StorageEngine;
///
/// let mut engine = StorageEngine::open("my_database").unwrap();
///
/// // Simple key-value operations
/// engine.put(b"name", b"Alice").unwrap();
/// let val = engine.get(b"name").unwrap();
/// assert_eq!(val, Some(b"Alice".to_vec()));
///
/// // Transactional operations
/// let txn = engine.begin_transaction().unwrap();
/// engine.txn_put(txn, b"key1", b"val1").unwrap();
/// engine.txn_put(txn, b"key2", b"val2").unwrap();
/// engine.commit_transaction(txn).unwrap();
/// ```
pub struct StorageEngine {
    btree: BPlusTree,
    pool: BufferPoolManager,
    wal: WalManager,
    txn_manager: TransactionManager,
    db_path: PathBuf,
}

impl StorageEngine {
    /// Open or create a database at the given path.
    ///
    /// This will create the directory if it doesn't exist, and initialize
    /// the data file, WAL, and all internal structures.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let db_path = path.as_ref().to_path_buf();

        // Create the directory if it doesn't exist
        std::fs::create_dir_all(&db_path)?;

        let data_file = db_path.join("data.db");
        let wal_file = db_path.join("wal.log");

        // Open WAL first (for recovery)
        let wal = WalManager::open(&wal_file)?;

        // Open or create the data file and buffer pool
        let disk = DiskManager::open(&data_file)?;
        let num_pages = disk.num_pages();
        let mut pool = BufferPoolManager::new(disk, DEFAULT_POOL_CAPACITY);

        let btree = if num_pages == 0 {
            BPlusTree::create(&mut pool)?
        } else {
            // Page 0 is always the root (or we could store root_id in a meta page)
            BPlusTree::open(0)
        };

        let txn_manager = TransactionManager::new();

        let mut engine = StorageEngine {
            btree,
            pool,
            wal,
            txn_manager,
            db_path,
        };

        // Perform crash recovery if needed
        engine.recover()?;

        Ok(engine)
    }

    /// Perform crash recovery using the WAL.
    fn recover(&mut self) -> Result<()> {
        let actions = self.wal.recover()?;

        if actions.redo.is_empty() && actions.undo.is_empty() {
            return Ok(());
        }

        eprintln!(
            "[storagedb] Recovery: {} redo actions, {} undo actions, {} committed, {} to abort",
            actions.redo.len(),
            actions.undo.len(),
            actions.committed_txns.len(),
            actions.aborted_txns.len()
        );

        let num_pages = self.pool.disk_manager().num_pages();

        // Phase 2: Redo — apply after-images for committed transactions
        for (page_id, after_image) in &actions.redo {
            if *page_id < num_pages {
                let page = self.pool.fetch_page_mut(*page_id)?;
                let copy_len = after_image.len().min(PAGE_SIZE);
                page.data[..copy_len].copy_from_slice(&after_image[..copy_len]);
                self.pool.unpin_page(*page_id, true)?;
            }
        }

        // Phase 3: Undo — apply before-images for uncommitted transactions
        for (page_id, before_image) in &actions.undo {
            if *page_id < num_pages {
                let page = self.pool.fetch_page_mut(*page_id)?;
                let copy_len = before_image.len().min(PAGE_SIZE);
                page.data[..copy_len].copy_from_slice(&before_image[..copy_len]);
                self.pool.unpin_page(*page_id, true)?;
            }
        }

        self.pool.flush_all()?;

        // Truncate WAL after successful recovery
        self.wal.truncate()?;

        Ok(())
    }

    // === Simple Key-Value API (auto-commit transactions) ===

    /// Put a key-value pair (auto-committed).
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.btree.insert(&mut self.pool, key, value)?;
        Ok(())
    }

    /// Get a value by key.
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.btree.search(&mut self.pool, key)
    }

    /// Delete a key. Returns the old value if it existed.
    pub fn delete(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.btree.delete(&mut self.pool, key)
    }

    /// Scan all key-value pairs in sorted order.
    pub fn scan_all(&mut self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.btree.scan_all(&mut self.pool)
    }

    /// Range scan: return all KV pairs where start <= key <= end.
    pub fn range_scan(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.btree.range_scan(&mut self.pool, start, end)
    }

    // === Transactional API ===

    /// Begin a new transaction. Returns the transaction ID.
    pub fn begin_transaction(&mut self) -> Result<u64> {
        self.txn_manager.begin(&mut self.wal)
    }

    /// Put a key-value pair within a transaction.
    pub fn txn_put(&mut self, txn_id: u64, key: &[u8], value: &[u8]) -> Result<()> {
        // For simplicity, we log at the key-value level rather than page level
        // A production engine would track page-level before/after images
        self.wal.log_page_write(txn_id, 0, key, value)?;
        self.btree.insert(&mut self.pool, key, value)?;
        Ok(())
    }

    /// Get a value within a transaction.
    pub fn txn_get(&mut self, _txn_id: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.btree.search(&mut self.pool, key)
    }

    /// Delete a key within a transaction.
    pub fn txn_delete(&mut self, txn_id: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let old = self.btree.delete(&mut self.pool, key)?;
        if let Some(ref val) = old {
            self.wal.log_page_write(txn_id, 0, key, val)?;
        }
        Ok(old)
    }

    /// Commit a transaction.
    pub fn commit_transaction(&mut self, txn_id: u64) -> Result<()> {
        self.txn_manager.commit(&mut self.wal, txn_id)
    }

    /// Abort a transaction.
    pub fn abort_transaction(&mut self, txn_id: u64) -> Result<()> {
        let _write_set = self.txn_manager.abort(&mut self.wal, txn_id)?;
        // In a full implementation, we'd use the write_set to restore before-images
        Ok(())
    }

    /// Write a checkpoint to the WAL.
    pub fn checkpoint(&mut self) -> Result<()> {
        let active = self.txn_manager.active_transaction_ids();
        self.wal.log_checkpoint(&active)?;
        Ok(())
    }

    /// Force sync all data to disk.
    pub fn sync(&mut self) -> Result<()> {
        self.wal.flush()?;
        self.pool.flush_all()?;
        Ok(())
    }

    /// Get database statistics.
    pub fn stats(&self) -> EngineStats {
        EngineStats {
            num_pages: self.pool.disk_manager().num_pages(),
            root_page_id: self.btree.root_page_id,
            active_transactions: self.txn_manager.active_count(),
            wal_lsn: self.wal.current_lsn(),
            db_path: self.db_path.to_string_lossy().to_string(),
        }
    }
}

/// Engine statistics
#[derive(Debug)]
pub struct EngineStats {
    pub num_pages: u32,
    pub root_page_id: u32,
    pub active_transactions: usize,
    pub wal_lsn: u64,
    pub db_path: String,
}

impl std::fmt::Display for EngineStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "StorageEngine[path={}, pages={}, root={}, active_txns={}, wal_lsn={}]",
            self.db_path, self.num_pages, self.root_page_id, self.active_transactions, self.wal_lsn
        )
    }
}
