use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::log_record::{LogRecord, LogRecordType, Lsn};
use crate::error::Result;

/// Manages the Write-Ahead Log file.
///
/// Responsibilities:
/// - Append log records to the WAL file
/// - Assign monotonically increasing LSNs
/// - Flush WAL to disk (force) before allowing dirty pages to be written
/// - Replay WAL for crash recovery (redo + undo)
///
/// Performance: Uses an in-memory write buffer to batch multiple WAL records
/// into a single write syscall. Records are buffered until flush() is called
/// (which happens on commit). This implements group commit — multiple records
/// from the same transaction are written in one I/O operation.
pub struct WalManager {
    file: File,
    path: PathBuf,
    next_lsn: AtomicU64,
    /// Current file offset (append position)
    offset: u64,
    /// Flushed LSN — all records up to this LSN are durable on disk
    flushed_lsn: u64,
    /// Map from txn_id -> last LSN for that transaction (for undo chain)
    txn_last_lsn: HashMap<u64, Lsn>,
    /// Write buffer — batches serialized records before flushing to disk.
    /// Reduces syscall overhead by coalescing multiple small writes into one.
    write_buffer: Vec<u8>,
}

impl WalManager {
    /// Open or create a WAL file
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        let file_len = file.metadata()?.len();

        // Scan existing records to find the max LSN
        let mut max_lsn: Lsn = 0;
        if file_len > 0 {
            let records = Self::read_all_records_from_file(&path_buf)?;
            for r in &records {
                if r.lsn > max_lsn {
                    max_lsn = r.lsn;
                }
            }
        }

        Ok(WalManager {
            file,
            path: path_buf,
            next_lsn: AtomicU64::new(max_lsn + 1),
            offset: file_len,
            flushed_lsn: max_lsn,
            txn_last_lsn: HashMap::new(),
            write_buffer: Vec::with_capacity(4096),
        })
    }

    /// Get the next LSN (without incrementing)
    pub fn current_lsn(&self) -> Lsn {
        self.next_lsn.load(Ordering::SeqCst)
    }

    /// Allocate and return the next LSN
    fn next_lsn(&self) -> Lsn {
        self.next_lsn.fetch_add(1, Ordering::SeqCst)
    }

    /// Append a log record to the write buffer. Returns the assigned LSN.
    /// Records are buffered in memory until flush() is called (on commit).
    /// This implements group commit — a single write + fsync for multiple records.
    pub fn append(&mut self, mut record: LogRecord) -> Result<Lsn> {
        let lsn = self.next_lsn();
        record.lsn = lsn;

        // Set prev_lsn from our tracking map
        if record.txn_id != 0 {
            record.prev_lsn = self.txn_last_lsn.get(&record.txn_id).copied().unwrap_or(0);
            self.txn_last_lsn.insert(record.txn_id, lsn);
        }

        let bytes = record.serialize();
        self.write_buffer.extend_from_slice(&bytes);
        self.offset += bytes.len() as u64;

        Ok(lsn)
    }

    /// Log a transaction begin
    pub fn log_begin(&mut self, txn_id: u64) -> Result<Lsn> {
        let record = LogRecord::begin(0, txn_id);
        self.append(record)
    }

    /// Log a transaction commit
    pub fn log_commit(&mut self, txn_id: u64) -> Result<Lsn> {
        let prev = self.txn_last_lsn.get(&txn_id).copied().unwrap_or(0);
        let record = LogRecord::commit(0, txn_id, prev);
        let lsn = self.append(record)?;
        // Force flush on commit for durability
        self.flush()?;
        // Clean up txn tracking
        self.txn_last_lsn.remove(&txn_id);
        Ok(lsn)
    }

    /// Log a transaction abort
    pub fn log_abort(&mut self, txn_id: u64) -> Result<Lsn> {
        let prev = self.txn_last_lsn.get(&txn_id).copied().unwrap_or(0);
        let record = LogRecord::abort(0, txn_id, prev);
        let lsn = self.append(record)?;
        self.txn_last_lsn.remove(&txn_id);
        Ok(lsn)
    }

    /// Log a page write (before/after image)
    pub fn log_page_write(
        &mut self,
        txn_id: u64,
        page_id: u32,
        before_image: &[u8],
        after_image: &[u8],
    ) -> Result<Lsn> {
        let prev = self.txn_last_lsn.get(&txn_id).copied().unwrap_or(0);
        let record = LogRecord::page_write(0, txn_id, prev, page_id, before_image, after_image);
        self.append(record)
    }

    /// Log a checkpoint
    pub fn log_checkpoint(&mut self, active_txns: &[u64]) -> Result<Lsn> {
        let record = LogRecord::checkpoint(0, active_txns);
        let lsn = self.append(record)?;
        self.flush()?;
        Ok(lsn)
    }

    /// Force flush the WAL write buffer and sync to disk.
    /// Drains the in-memory buffer in a single write syscall, then calls
    /// sync_data() (fdatasync) for durability. This is where the group commit
    /// optimization pays off — all buffered records go out in one I/O.
    pub fn flush(&mut self) -> Result<()> {
        if !self.write_buffer.is_empty() {
            self.file.seek(SeekFrom::Start(
                self.offset - self.write_buffer.len() as u64,
            ))?;
            self.file.write_all(&self.write_buffer)?;
            self.write_buffer.clear();
        }
        self.file.sync_data()?;
        self.flushed_lsn = self.next_lsn.load(Ordering::SeqCst) - 1;
        Ok(())
    }

    /// Get the flushed LSN
    pub fn flushed_lsn(&self) -> Lsn {
        self.flushed_lsn
    }

    /// Read all log records from the WAL file (for recovery)
    fn read_all_records_from_file(path: &Path) -> Result<Vec<LogRecord>> {
        let mut file = File::open(path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;

        let mut records = Vec::new();
        let mut pos = 0;

        while pos < buf.len() {
            // Try to read a record; if corrupted (e.g., torn write), stop
            match LogRecord::deserialize(&buf[pos..]) {
                Ok((record, consumed)) => {
                    records.push(record);
                    pos += consumed;
                }
                Err(_) => {
                    // Corrupted record — stop reading (torn write at end)
                    break;
                }
            }
        }

        Ok(records)
    }

    /// Read all log records from the current WAL file
    pub fn read_all_records(&self) -> Result<Vec<LogRecord>> {
        Self::read_all_records_from_file(&self.path)
    }

    /// Perform ARIES-style crash recovery.
    ///
    /// Returns a list of (page_id, after_image) pairs that need to be applied (redo),
    /// and a list of (page_id, before_image) pairs that need to be undone.
    ///
    /// Phase 1 (Analysis): Determine which transactions committed and which didn't.
    /// Phase 2 (Redo): Replay all changes from committed transactions.
    /// Phase 3 (Undo): Reverse all changes from uncommitted transactions.
    pub fn recover(&self) -> Result<RecoveryActions> {
        let records = self.read_all_records()?;

        if records.is_empty() {
            return Ok(RecoveryActions::default());
        }

        // Phase 1: Analysis — track which transactions committed
        let mut committed: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut aborted: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut active: std::collections::HashSet<u64> = std::collections::HashSet::new();

        for record in &records {
            match record.record_type {
                LogRecordType::Begin => {
                    active.insert(record.txn_id);
                }
                LogRecordType::Commit => {
                    committed.insert(record.txn_id);
                    active.remove(&record.txn_id);
                }
                LogRecordType::Abort => {
                    aborted.insert(record.txn_id);
                    active.remove(&record.txn_id);
                }
                _ => {}
            }
        }

        // Phase 2: Redo — collect page writes from committed transactions
        let mut redo_actions: Vec<(u32, Vec<u8>)> = Vec::new();
        for record in &records {
            if record.record_type == LogRecordType::PageWrite && committed.contains(&record.txn_id)
            {
                if let Some((_, after)) = record.page_write_images() {
                    redo_actions.push((record.page_id, after.to_vec()));
                }
            }
        }

        // Phase 3: Undo — collect page writes from uncommitted (active) transactions, in reverse
        let mut undo_actions: Vec<(u32, Vec<u8>)> = Vec::new();
        for record in records.iter().rev() {
            if record.record_type == LogRecordType::PageWrite && active.contains(&record.txn_id) {
                if let Some((before, _)) = record.page_write_images() {
                    undo_actions.push((record.page_id, before.to_vec()));
                }
            }
        }

        Ok(RecoveryActions {
            redo: redo_actions,
            undo: undo_actions,
            committed_txns: committed.into_iter().collect(),
            aborted_txns: active.into_iter().collect(), // active at crash = need abort
        })
    }

    /// Truncate the WAL file (after a successful checkpoint)
    pub fn truncate(&mut self) -> Result<()> {
        self.write_buffer.clear();
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.offset = 0;
        self.txn_last_lsn.clear();
        Ok(())
    }
}

/// Result of crash recovery analysis
#[derive(Debug, Default)]
pub struct RecoveryActions {
    /// Pages to redo: (page_id, after_image)
    pub redo: Vec<(u32, Vec<u8>)>,
    /// Pages to undo: (page_id, before_image)
    pub undo: Vec<(u32, Vec<u8>)>,
    /// Transaction IDs that committed successfully
    pub committed_txns: Vec<u64>,
    /// Transaction IDs that need to be aborted (were active at crash)
    pub aborted_txns: Vec<u64>,
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
            let p = std::env::temp_dir().join(format!("storagedb_wal_test_{}", id));
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

    #[test]
    fn test_wal_basic_operations() {
        let dir = TempDir::new();
        let wal_path = dir.path().join("test.wal");

        let mut wal = WalManager::open(&wal_path).unwrap();

        let lsn1 = wal.log_begin(1).unwrap();
        assert_eq!(lsn1, 1);

        let lsn2 = wal.log_page_write(1, 10, b"old", b"new").unwrap();
        assert_eq!(lsn2, 2);

        let lsn3 = wal.log_commit(1).unwrap();
        assert_eq!(lsn3, 3);

        // Read back
        let records = wal.read_all_records().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].record_type, LogRecordType::Begin);
        assert_eq!(records[1].record_type, LogRecordType::PageWrite);
        assert_eq!(records[2].record_type, LogRecordType::Commit);
    }

    #[test]
    fn test_recovery_committed_transaction() {
        let dir = TempDir::new();
        let wal_path = dir.path().join("test.wal");

        {
            let mut wal = WalManager::open(&wal_path).unwrap();
            wal.log_begin(1).unwrap();
            wal.log_page_write(1, 10, b"before", b"after").unwrap();
            wal.log_commit(1).unwrap();
        }

        // Simulate crash and recovery
        let wal = WalManager::open(&wal_path).unwrap();
        let actions = wal.recover().unwrap();

        assert_eq!(actions.redo.len(), 1);
        assert_eq!(actions.redo[0].0, 10);
        assert_eq!(actions.redo[0].1, b"after".to_vec());
        assert!(actions.undo.is_empty());
        assert!(actions.committed_txns.contains(&1));
    }

    #[test]
    fn test_recovery_uncommitted_transaction() {
        let dir = TempDir::new();
        let wal_path = dir.path().join("test.wal");

        {
            let mut wal = WalManager::open(&wal_path).unwrap();
            wal.log_begin(1).unwrap();
            wal.log_page_write(1, 10, b"original", b"modified").unwrap();
            // NO commit — simulate crash
            wal.flush().unwrap();
        }

        let wal = WalManager::open(&wal_path).unwrap();
        let actions = wal.recover().unwrap();

        assert!(actions.redo.is_empty());
        assert_eq!(actions.undo.len(), 1);
        assert_eq!(actions.undo[0].0, 10);
        assert_eq!(actions.undo[0].1, b"original".to_vec());
        assert!(actions.aborted_txns.contains(&1));
    }

    #[test]
    fn test_recovery_mixed_transactions() {
        let dir = TempDir::new();
        let wal_path = dir.path().join("test.wal");

        {
            let mut wal = WalManager::open(&wal_path).unwrap();

            // Txn 1: committed
            wal.log_begin(1).unwrap();
            wal.log_page_write(1, 10, b"old1", b"new1").unwrap();
            wal.log_commit(1).unwrap();

            // Txn 2: uncommitted (crash)
            wal.log_begin(2).unwrap();
            wal.log_page_write(2, 20, b"old2", b"new2").unwrap();
            wal.flush().unwrap();
        }

        let wal = WalManager::open(&wal_path).unwrap();
        let actions = wal.recover().unwrap();

        assert_eq!(actions.redo.len(), 1);
        assert_eq!(actions.redo[0].0, 10);
        assert_eq!(actions.undo.len(), 1);
        assert_eq!(actions.undo[0].0, 20);
    }

    #[test]
    fn test_wal_reopen() {
        let dir = TempDir::new();
        let wal_path = dir.path().join("test.wal");

        {
            let mut wal = WalManager::open(&wal_path).unwrap();
            wal.log_begin(1).unwrap();
            wal.log_commit(1).unwrap();
        }

        // Reopen — LSN should continue
        let mut wal = WalManager::open(&wal_path).unwrap();
        let lsn = wal.log_begin(2).unwrap();
        assert_eq!(lsn, 3);
    }

    #[test]
    fn test_wal_truncate() {
        let dir = TempDir::new();
        let wal_path = dir.path().join("test.wal");

        let mut wal = WalManager::open(&wal_path).unwrap();
        wal.log_begin(1).unwrap();
        wal.log_commit(1).unwrap();
        wal.truncate().unwrap();

        let records = wal.read_all_records().unwrap();
        assert!(records.is_empty());
    }
}
