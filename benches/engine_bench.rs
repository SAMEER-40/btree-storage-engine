//! Simple benchmarks for the storage engine.
//! Run with: cargo bench
//!
//! These use a straightforward timing approach since we avoid external dependencies.

use storagedb::StorageEngine;

fn bench_sequential_insert(sizes: &[usize]) {
    println!("=== Sequential Insert Benchmark ===");
    for &size in sizes {
        let db_path = std::env::temp_dir().join(format!("storagedb_bench_seq_{}", size));
        let _ = std::fs::remove_dir_all(&db_path);

        let mut engine = StorageEngine::open(&db_path).unwrap();

        let start = std::time::Instant::now();
        for i in 0..size {
            let key = format!("key_{:08}", i);
            let val = format!("val_{:08}", i);
            engine.put(key.as_bytes(), val.as_bytes()).unwrap();
        }
        let elapsed = start.elapsed();
        let ops_per_sec = size as f64 / elapsed.as_secs_f64();

        println!(
            "  n={:>6}: {:.2?} ({:.0} ops/sec)",
            size, elapsed, ops_per_sec
        );

        let _ = std::fs::remove_dir_all(&db_path);
    }
    println!();
}

fn bench_random_lookup(n: usize, lookups: usize) {
    println!(
        "=== Random Lookup Benchmark (n={}, lookups={}) ===",
        n, lookups
    );
    let db_path = std::env::temp_dir().join("storagedb_bench_lookup");
    let _ = std::fs::remove_dir_all(&db_path);

    let mut engine = StorageEngine::open(&db_path).unwrap();

    // Pre-populate
    for i in 0..n {
        let key = format!("key_{:08}", i);
        let val = format!("val_{:08}", i);
        engine.put(key.as_bytes(), val.as_bytes()).unwrap();
    }

    let start = std::time::Instant::now();
    let mut idx = 0u64;
    for _ in 0..lookups {
        let key = format!("key_{:08}", idx % n as u64);
        let _ = engine.get(key.as_bytes()).unwrap();
        idx = idx.wrapping_add(7919); // prime step for pseudo-random
    }
    let elapsed = start.elapsed();
    let ops_per_sec = lookups as f64 / elapsed.as_secs_f64();

    println!("  {:.2?} ({:.0} ops/sec)", elapsed, ops_per_sec);

    let _ = std::fs::remove_dir_all(&db_path);
    println!();
}

fn bench_range_scan(n: usize, scans: usize) {
    println!("=== Range Scan Benchmark (n={}, scans={}) ===", n, scans);
    let db_path = std::env::temp_dir().join("storagedb_bench_range");
    let _ = std::fs::remove_dir_all(&db_path);

    let mut engine = StorageEngine::open(&db_path).unwrap();

    for i in 0..n {
        let key = format!("key_{:08}", i);
        let val = format!("val_{:08}", i);
        engine.put(key.as_bytes(), val.as_bytes()).unwrap();
    }

    let start = std::time::Instant::now();
    for _ in 0..scans {
        let _ = engine.range_scan(b"key_00001000", b"key_00001100").unwrap();
    }
    let elapsed = start.elapsed();
    let ops_per_sec = scans as f64 / elapsed.as_secs_f64();

    println!("  {:.2?} ({:.0} scans/sec)", elapsed, ops_per_sec);

    let _ = std::fs::remove_dir_all(&db_path);
    println!();
}

fn bench_mixed_workload(n_prepopulate: usize, operations: usize) {
    println!(
        "=== Mixed Workload Benchmark (prepop={}, ops={}) ===",
        n_prepopulate, operations
    );
    let db_path = std::env::temp_dir().join("storagedb_bench_mixed");
    let _ = std::fs::remove_dir_all(&db_path);

    let mut engine = StorageEngine::open(&db_path).unwrap();

    // Pre-populate
    for i in 0..n_prepopulate {
        let key = format!("key_{:08}", i);
        let val = format!("val_{:08}", i);
        engine.put(key.as_bytes(), val.as_bytes()).unwrap();
    }

    let start = std::time::Instant::now();
    for i in 0..operations {
        if i % 5 == 0 {
            // 20% writes
            let key = format!("key_{:08}", i % n_prepopulate);
            let val = format!("updated_{:08}", i);
            engine.put(key.as_bytes(), val.as_bytes()).unwrap();
        } else {
            // 80% reads
            let key = format!("key_{:08}", (i.wrapping_mul(7919)) % n_prepopulate);
            let _ = engine.get(key.as_bytes()).unwrap();
        }
    }
    let elapsed = start.elapsed();
    let ops_per_sec = operations as f64 / elapsed.as_secs_f64();

    println!("  {:.2?} ({:.0} ops/sec)", elapsed, ops_per_sec);

    let _ = std::fs::remove_dir_all(&db_path);
    println!();
}

fn main() {
    println!("StorageDB Benchmarks");
    println!("====================");
    println!();

    bench_sequential_insert(&[100, 1000, 5000]);
    bench_random_lookup(10000, 50000);
    bench_range_scan(10000, 1000);
    bench_mixed_workload(1000, 10000);

    println!("All benchmarks complete.");
}
