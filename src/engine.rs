use std::path::{Path, PathBuf};

use crate::btree::BPlusTree;
use crate::disk::disk_manager::DiskManager;
use crate::disk::page::PAGE_SIZE;
use crate::error::Result;
use crate::txn::txn_manager::TransactionManager;
use crate::wal::wal_manager::WalManager;

/// The main storage engine.
///
/// Usage:
/// \`\`\`no_run
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
/// \`\`\`
pub struct StorageEngine {
    btree: BPlusTree,
    wal: WalManager,
    txn_manager: TransactionManager,
    db_path: PathBuf,
}

impl StorageEngine {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let db_path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&db_path)?;

        let data_file = db_path.join("data.db");
        let wal_file = db_path.join("wal.log");

        let wal = WalManager::open(&wal_file)?;

        let disk = DiskManager::open(&data_file)?;
        let btree = if disk.num_pages() == 0 {
            BPlusTree::create(disk)?
        } else {
            BPlusTree::open(disk, 0)
        };

        let txn_manager = TransactionManager::new();

        let mut engine = StorageEngine {
            btree,
            wal,
            txn_manager,
            db_path,
        };

        engine.recover()?;
        Ok(engine)
    }

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

        for (page_id, after_image) in &actions.redo {
            if *page_id < self.btree.disk_manager().num_pages() {
                let mut page = self.btree.disk_manager_mut().read_page(*page_id)?;
                let copy_len = after_image.len().min(PAGE_SIZE);
                page.data[..copy_len].copy_from_slice(&after_image[..copy_len]);
                self.btree.disk_manager_mut().write_page(&page)?;
            }
        }

        for (page_id, before_image) in &actions.undo {
            if *page_id < self.btree.disk_manager().num_pages() {
                let mut page = self.btree.disk_manager_mut().read_page(*page_id)?;
                let copy_len = before_image.len().min(PAGE_SIZE);
                page.data[..copy_len].copy_from_slice(&before_image[..copy_len]);
                self.btree.disk_manager_mut().write_page(&page)?;
            }
        }

        self.btree.disk_manager_mut().sync()?;
        self.wal.truncate()?;
        Ok(())
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.btree.insert(key, value)?;
        Ok(())
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.btree.search(key)
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.btree.delete(key)
    }

    pub fn scan_all(&mut self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.btree.scan_all()
    }

    pub fn range_scan(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.btree.range_scan(start, end)
    }

    pub fn begin_transaction(&mut self) -> Result<u64> {
        self.txn_manager.begin(&mut self.wal)
    }

    pub fn txn_put(&mut self, txn_id: u64, key: &[u8], value: &[u8]) -> Result<()> {
        self.wal.log_page_write(txn_id, 0, key, value)?;
        self.btree.insert(key, value)?;
        Ok(())
    }

    pub fn txn_get(&mut self, _txn_id: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.btree.search(key)
    }

    pub fn txn_delete(&mut self, txn_id: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let old = self.btree.delete(key)?;
        if let Some(ref val) = old {
            self.wal.log_page_write(txn_id, 0, key, val)?;
        }
        Ok(old)
    }

    pub fn commit_transaction(&mut self, txn_id: u64) -> Result<()> {
        self.txn_manager.commit(&mut self.wal, txn_id)
    }

    pub fn abort_transaction(&mut self, txn_id: u64) -> Result<()> {
        let _write_set = self.txn_manager.abort(&mut self.wal, txn_id)?;
        Ok(())
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        let active = self.txn_manager.active_transaction_ids();
        self.wal.log_checkpoint(&active)?;
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.wal.flush()?;
        self.btree.disk_manager_mut().sync()?;
        Ok(())
    }

    pub fn stats(&self) -> EngineStats {
        EngineStats {
            num_pages: self.btree.disk_manager().num_pages(),
            root_page_id: self.btree.root_page_id,
            active_transactions: self.txn_manager.active_count(),
            wal_lsn: self.wal.current_lsn(),
            db_path: self.db_path.to_string_lossy().to_string(),
        }
    }
}

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
