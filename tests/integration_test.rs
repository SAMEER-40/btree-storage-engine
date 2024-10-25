use storagedb::StorageEngine;

use std::sync::atomic::{AtomicUsize, Ordering};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn test_dir(name: &str) -> std::path::PathBuf {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("storagedb_test_{}_{}", name, id));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        TempDir(test_dir(name))
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

/// Integration test: full engine lifecycle
#[test]
fn test_engine_basic_crud() {
    let dir = TempDir::new("crud");
    let db_path = dir.path().join("test_db");

    let mut engine = StorageEngine::open(&db_path).unwrap();

    // Insert
    engine.put(b"hello", b"world").unwrap();
    engine.put(b"foo", b"bar").unwrap();
    engine.put(b"rust", b"fast").unwrap();

    // Read
    assert_eq!(engine.get(b"hello").unwrap(), Some(b"world".to_vec()));
    assert_eq!(engine.get(b"foo").unwrap(), Some(b"bar".to_vec()));
    assert_eq!(engine.get(b"missing").unwrap(), None);

    // Update
    engine.put(b"hello", b"universe").unwrap();
    assert_eq!(engine.get(b"hello").unwrap(), Some(b"universe".to_vec()));

    // Delete
    let old = engine.delete(b"foo").unwrap();
    assert_eq!(old, Some(b"bar".to_vec()));
    assert_eq!(engine.get(b"foo").unwrap(), None);
}

/// Integration test: scan operations
#[test]
fn test_engine_scan() {
    let dir = TempDir::new("scan");
    let db_path = dir.path().join("test_db");

    let mut engine = StorageEngine::open(&db_path).unwrap();

    for i in 0..100 {
        let key = format!("key_{:04}", i);
        let val = format!("val_{:04}", i);
        engine.put(key.as_bytes(), val.as_bytes()).unwrap();
    }

    // Full scan
    let all = engine.scan_all().unwrap();
    assert_eq!(all.len(), 100);
    // Should be sorted
    for i in 0..99 {
        assert!(all[i].0 < all[i + 1].0);
    }

    // Range scan
    let range = engine.range_scan(b"key_0020", b"key_0030").unwrap();
    assert_eq!(range.len(), 11);
}

/// Integration test: large dataset with splits
#[test]
fn test_engine_large_dataset() {
    let dir = TempDir::new("large");
    let db_path = dir.path().join("test_db");

    let mut engine = StorageEngine::open(&db_path).unwrap();

    let n = 5000;
    for i in 0..n {
        let key = format!("k{:06}", i);
        let val = format!("v{:06}", i);
        engine.put(key.as_bytes(), val.as_bytes()).unwrap();
    }

    // Verify all entries
    for i in 0..n {
        let key = format!("k{:06}", i);
        let val = format!("v{:06}", i);
        let result = engine.get(key.as_bytes()).unwrap();
        assert_eq!(
            result,
            Some(val.as_bytes().to_vec()),
            "Failed at key k{:06}",
            i
        );
    }

    // Verify sorted order
    let all = engine.scan_all().unwrap();
    assert_eq!(all.len(), n);
    for i in 0..n - 1 {
        assert!(all[i].0 < all[i + 1].0, "Not sorted at index {}", i);
    }
}

/// Integration test: persistence across reopens
#[test]
fn test_engine_persistence() {
    let dir = TempDir::new("persist");
    let db_path = dir.path().join("test_db");

    // Write data
    {
        let mut engine = StorageEngine::open(&db_path).unwrap();
        engine.put(b"persistent_key", b"persistent_value").unwrap();
        engine.put(b"another_key", b"another_value").unwrap();
        engine.sync().unwrap();
    }

    // Reopen and verify
    {
        let mut engine = StorageEngine::open(&db_path).unwrap();
        assert_eq!(
            engine.get(b"persistent_key").unwrap(),
            Some(b"persistent_value".to_vec())
        );
        assert_eq!(
            engine.get(b"another_key").unwrap(),
            Some(b"another_value".to_vec())
        );
    }
}

/// Integration test: transactions
#[test]
fn test_engine_transactions() {
    let dir = TempDir::new("txn");
    let db_path = dir.path().join("test_db");

    let mut engine = StorageEngine::open(&db_path).unwrap();

    // Committed transaction
    let txn = engine.begin_transaction().unwrap();
    engine.txn_put(txn, b"tx_key1", b"tx_val1").unwrap();
    engine.txn_put(txn, b"tx_key2", b"tx_val2").unwrap();
    engine.commit_transaction(txn).unwrap();

    assert_eq!(engine.get(b"tx_key1").unwrap(), Some(b"tx_val1".to_vec()));
    assert_eq!(engine.get(b"tx_key2").unwrap(), Some(b"tx_val2".to_vec()));
}

/// Integration test: interleaved insert and delete
#[test]
fn test_engine_insert_delete_interleaved() {
    let dir = TempDir::new("interleaved");
    let db_path = dir.path().join("test_db");

    let mut engine = StorageEngine::open(&db_path).unwrap();

    // Insert 100, delete even, verify odd remain
    for i in 0..100 {
        let key = format!("item_{:04}", i);
        let val = format!("data_{:04}", i);
        engine.put(key.as_bytes(), val.as_bytes()).unwrap();
    }

    for i in (0..100).step_by(2) {
        let key = format!("item_{:04}", i);
        engine.delete(key.as_bytes()).unwrap();
    }

    for i in 0..100 {
        let key = format!("item_{:04}", i);
        let result = engine.get(key.as_bytes()).unwrap();
        if i % 2 == 0 {
            assert_eq!(result, None, "Should be deleted: {}", key);
        } else {
            let val = format!("data_{:04}", i);
            assert_eq!(
                result,
                Some(val.as_bytes().to_vec()),
                "Should exist: {}",
                key
            );
        }
    }

    let all = engine.scan_all().unwrap();
    assert_eq!(all.len(), 50);
}

/// Integration test: WAL recovery
#[test]
fn test_wal_basic_recovery() {
    let dir = TempDir::new("wal");
    let wal_path = dir.path().join("test.wal");

    use storagedb::wal::WalManager;

    // Write some records
    {
        let mut wal = WalManager::open(&wal_path).unwrap();
        wal.log_begin(1).unwrap();
        wal.log_page_write(1, 10, b"old_data", b"new_data").unwrap();
        wal.log_commit(1).unwrap();

        // Uncommitted transaction
        wal.log_begin(2).unwrap();
        wal.log_page_write(2, 20, b"before", b"after").unwrap();
        wal.flush().unwrap();
    }

    // Recovery
    let wal = WalManager::open(&wal_path).unwrap();
    let actions = wal.recover().unwrap();

    // Txn 1 committed — redo its writes
    assert_eq!(actions.redo.len(), 1);
    assert_eq!(actions.redo[0].0, 10);

    // Txn 2 uncommitted — undo its writes
    assert_eq!(actions.undo.len(), 1);
    assert_eq!(actions.undo[0].0, 20);
}

/// Integration test: engine stats
#[test]
fn test_engine_stats() {
    let dir = TempDir::new("stats");
    let db_path = dir.path().join("test_db");

    let mut engine = StorageEngine::open(&db_path).unwrap();
    let stats = engine.stats();

    assert!(stats.num_pages >= 1);
    assert_eq!(stats.active_transactions, 0);

    engine.put(b"k", b"v").unwrap();
    let stats = engine.stats();
    assert!(stats.num_pages >= 1);
}
