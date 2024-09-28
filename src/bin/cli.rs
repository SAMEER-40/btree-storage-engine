use std::io::{self, BufRead, Write};
use storagedb::StorageEngine;

fn main() {
    let db_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "storagedb_data".to_string());

    println!("StorageDB - A database storage engine");
    println!("Opening database at: {}", db_path);

    let mut engine = match StorageEngine::open(&db_path) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to open database: {}", e);
            std::process::exit(1);
        }
    };

    println!("{}", engine.stats());
    println!();
    println!("Commands:");
    println!("  put <key> <value>   - Store a key-value pair");
    println!("  get <key>           - Retrieve a value by key");
    println!("  del <key>           - Delete a key");
    println!("  scan                - List all key-value pairs");
    println!("  range <start> <end> - Range scan");
    println!("  stats               - Show database statistics");
    println!("  bench <n>           - Insert n records and measure throughput");
    println!("  quit                - Exit");
    println!();

    print!("> ");
    io::stdout().flush().ok();

    let stdin = io::stdin();
    let reader = stdin.lock();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        let parts: Vec<&str> = line.trim().splitn(3, ' ').collect();
        if parts.is_empty() || parts[0].is_empty() {
            print!("> ");
            io::stdout().flush().ok();
            continue;
        }

        match parts[0] {
            "put" => {
                if parts.len() < 3 {
                    println!("Usage: put <key> <value>");
                } else {
                    match engine.put(parts[1].as_bytes(), parts[2].as_bytes()) {
                        Ok(()) => println!("OK"),
                        Err(e) => println!("Error: {}", e),
                    }
                }
            }
            "get" => {
                if parts.len() < 2 {
                    println!("Usage: get <key>");
                } else {
                    match engine.get(parts[1].as_bytes()) {
                        Ok(Some(val)) => match std::str::from_utf8(&val) {
                            Ok(s) => println!("{}", s),
                            Err(_) => println!("{:?}", val),
                        },
                        Ok(None) => println!("(not found)"),
                        Err(e) => println!("Error: {}", e),
                    }
                }
            }
            "del" => {
                if parts.len() < 2 {
                    println!("Usage: del <key>");
                } else {
                    match engine.delete(parts[1].as_bytes()) {
                        Ok(Some(val)) => match std::str::from_utf8(&val) {
                            Ok(s) => println!("Deleted: {}", s),
                            Err(_) => println!("Deleted: {:?}", val),
                        },
                        Ok(None) => println!("(not found)"),
                        Err(e) => println!("Error: {}", e),
                    }
                }
            }
            "scan" => match engine.scan_all() {
                Ok(entries) => {
                    if entries.is_empty() {
                        println!("(empty)");
                    } else {
                        for (k, v) in &entries {
                            let ks = std::str::from_utf8(k).unwrap_or("(binary)");
                            let vs = std::str::from_utf8(v).unwrap_or("(binary)");
                            println!("  {} => {}", ks, vs);
                        }
                        println!("({} entries)", entries.len());
                    }
                }
                Err(e) => println!("Error: {}", e),
            },
            "range" => {
                if parts.len() < 3 {
                    println!("Usage: range <start> <end>");
                } else {
                    match engine.range_scan(parts[1].as_bytes(), parts[2].as_bytes()) {
                        Ok(entries) => {
                            for (k, v) in &entries {
                                let ks = std::str::from_utf8(k).unwrap_or("(binary)");
                                let vs = std::str::from_utf8(v).unwrap_or("(binary)");
                                println!("  {} => {}", ks, vs);
                            }
                            println!("({} entries)", entries.len());
                        }
                        Err(e) => println!("Error: {}", e),
                    }
                }
            }
            "stats" => {
                println!("{}", engine.stats());
            }
            "bench" => {
                let n: usize = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(10000);

                println!("Inserting {} records...", n);
                let start = std::time::Instant::now();

                for i in 0..n {
                    let key = format!("bench_key_{:08}", i);
                    let val = format!("bench_val_{:08}", i);
                    if let Err(e) = engine.put(key.as_bytes(), val.as_bytes()) {
                        println!("Error at {}: {}", i, e);
                        break;
                    }
                }

                let elapsed = start.elapsed();
                let throughput = n as f64 / elapsed.as_secs_f64();
                println!(
                    "Inserted {} records in {:.2?} ({:.0} ops/sec)",
                    n, elapsed, throughput
                );

                // Verify a sample
                let sample_key = format!("bench_key_{:08}", n / 2);
                let sample_val = format!("bench_val_{:08}", n / 2);
                match engine.get(sample_key.as_bytes()) {
                    Ok(Some(v)) if v == sample_val.as_bytes() => {
                        println!("Verification: OK");
                    }
                    Ok(Some(v)) => {
                        println!(
                            "Verification: MISMATCH (expected {:?}, got {:?})",
                            sample_val.as_bytes(),
                            v
                        );
                    }
                    Ok(None) => println!("Verification: MISSING"),
                    Err(e) => println!("Verification error: {}", e),
                }
            }
            "quit" | "exit" | "q" => {
                println!("Syncing...");
                engine.sync().ok();
                println!("Goodbye!");
                break;
            }
            _ => {
                println!("Unknown command: {}", parts[0]);
            }
        }

        print!("> ");
        io::stdout().flush().ok();
    }
}
