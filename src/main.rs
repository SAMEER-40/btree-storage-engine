use storagedb::StorageEngine;

fn main() {
    println!("=== StorageDB Demo ===");
    println!();

    // Create a temporary database directory
    let db_path = std::env::temp_dir().join("storagedb_demo");
    // Clean up any previous run
    let _ = std::fs::remove_dir_all(&db_path);

    let mut engine = StorageEngine::open(&db_path).expect("Failed to open database");
    println!("Database opened: {}", engine.stats());

    // Insert some data
    println!();
    println!("--- Inserting data ---");
    let entries = vec![
        ("alice", "software engineer"),
        ("bob", "data scientist"),
        ("charlie", "devops engineer"),
        ("diana", "product manager"),
        ("eve", "security researcher"),
    ];

    for (k, v) in &entries {
        engine.put(k.as_bytes(), v.as_bytes()).unwrap();
        println!("  PUT {} => {}", k, v);
    }

    // Lookup
    println!();
    println!("--- Lookups ---");
    for name in &["alice", "charlie", "frank"] {
        match engine.get(name.as_bytes()).unwrap() {
            Some(val) => println!("  GET {} => {}", name, std::str::from_utf8(&val).unwrap()),
            None => println!("  GET {} => (not found)", name),
        }
    }

    // Update
    println!();
    println!("--- Update ---");
    engine.put(b"alice", b"CTO").unwrap();
    let val = engine.get(b"alice").unwrap().unwrap();
    println!("  alice is now: {}", std::str::from_utf8(&val).unwrap());

    // Delete
    println!();
    println!("--- Delete ---");
    let old = engine.delete(b"bob").unwrap();
    println!(
        "  Deleted bob (was: {})",
        std::str::from_utf8(&old.unwrap()).unwrap()
    );

    // Scan all
    println!();
    println!("--- Scan all (sorted) ---");
    let all = engine.scan_all().unwrap();
    for (k, v) in &all {
        println!(
            "  {} => {}",
            std::str::from_utf8(k).unwrap(),
            std::str::from_utf8(v).unwrap()
        );
    }

    // Range scan
    println!();
    println!("--- Range scan [c, e] ---");
    let range = engine.range_scan(b"c", b"e").unwrap();
    for (k, v) in &range {
        println!(
            "  {} => {}",
            std::str::from_utf8(k).unwrap(),
            std::str::from_utf8(v).unwrap()
        );
    }

    // Benchmark
    println!();
    println!("--- Benchmark: 10,000 inserts ---");
    let start = std::time::Instant::now();
    for i in 0..10_000 {
        let key = format!("bench_{:06}", i);
        let val = format!("value_{:06}", i);
        engine.put(key.as_bytes(), val.as_bytes()).unwrap();
    }
    let elapsed = start.elapsed();
    println!(
        "  Inserted 10,000 records in {:.2?} ({:.0} ops/sec)",
        elapsed,
        10_000.0 / elapsed.as_secs_f64()
    );

    // Verify
    let verify_key = b"bench_005000";
    let verify_val = engine.get(verify_key).unwrap().unwrap();
    println!(
        "  Verified: bench_005000 => {}",
        std::str::from_utf8(&verify_val).unwrap()
    );

    // Transaction demo
    println!();
    println!("--- Transaction demo ---");
    let txn = engine.begin_transaction().unwrap();
    println!("  BEGIN txn {}", txn);
    engine.txn_put(txn, b"txn_key1", b"txn_val1").unwrap();
    engine.txn_put(txn, b"txn_key2", b"txn_val2").unwrap();
    engine.commit_transaction(txn).unwrap();
    println!("  COMMIT txn {}", txn);

    let v1 = engine.get(b"txn_key1").unwrap().unwrap();
    let v2 = engine.get(b"txn_key2").unwrap().unwrap();
    println!(
        "  txn_key1 => {}, txn_key2 => {}",
        std::str::from_utf8(&v1).unwrap(),
        std::str::from_utf8(&v2).unwrap()
    );

    // Final stats
    println!();
    println!("--- Final stats ---");
    println!("  {}", engine.stats());

    engine.sync().unwrap();

    // Clean up
    let _ = std::fs::remove_dir_all(&db_path);

    println!();
    println!("Done!");
}
