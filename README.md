# storagedb

a database storage engine built from scratch in rust. zero external dependencies — everything is hand-rolled, including CRC32 checksums.

started this as a learning project to understand how databases actually work under the hood. turns out it's a lot of page management and careful crash recovery logic.

## what's in here

- **B+ tree index** — the main data structure. supports insert, delete, point lookup, range scan. splits leaf and internal nodes when they fill up. keys and values are variable-length byte slices stored in a slotted page layout.
- **buffer pool** — caches disk pages in memory with CLOCK eviction (approximates LRU without the overhead). configurable pool size, pin/unpin semantics, dirty page tracking.
- **write-ahead log (WAL)** — ARIES-style recovery with redo/undo phases. log records are checksummed. supports group commit via write buffering.
- **transaction manager** — begin/commit/abort with write-set tracking. uses WAL for atomicity and durability.
- **lock manager** — strict two-phase locking (S2PL) with shared and exclusive modes. deadlock avoidance via conflict detection.
- **disk manager** — page-level I/O with sparse file allocation. uses `fdatasync` instead of `fsync` to skip unnecessary metadata flushes.

## architecture

```
┌─────────────────────────────────┐
│         StorageEngine           │
│  (put/get/delete/scan/txn ops)  │
├─────────────────────────────────┤
│    B+ Tree    │   Transaction   │
│    Index      │    Manager      │
├───────────────┼─────────────────┤
│  Buffer Pool  │  Lock Manager   │
│  (CLOCK)      │  (S2PL)         │
├───────────────┴─────────────────┤
│         WAL Manager             │
├─────────────────────────────────┤
│         Disk Manager            │
│      (4KB page I/O)             │
└─────────────────────────────────┘
```

## building & running

```bash
cargo build
cargo test        # 56 tests (unit + integration + doc)
cargo bench       # sequential insert, random lookup, range scan, mixed workload

# run the demo
cargo run

# interactive CLI
cargo run --bin cli
```

the CLI supports commands like `put key value`, `get key`, `del key`, `scan`, `range start end`, `stats`, and `bench`.

## performance

after a few rounds of optimization (removing per-write flushes, binary search in nodes, buffer pool, WAL batching):

| workload | ops/sec |
|---|---|
| sequential insert | ~118k |
| random lookup | ~454k |
| range scan | ~59k |
| mixed (70/30 read/write) | ~143k |

the buffer pool was the biggest win — 3.5x improvement on random lookups since hot pages (root, upper internal nodes) stay cached.

## page layout

every page is 4KB with a fixed header:

```
[page_id: 4B][page_type: 1B][num_slots: 2B][free_offset: 2B][checksum: 4B]
```

B+ tree nodes use a slotted page design — slot directory grows forward from the header, key/value data grows backward from the end of the page. this lets us do inserts without compaction most of the time.

## what i learned

- crash recovery is genuinely hard. the WAL needs to handle partially written records, and you need to think carefully about what "committed" actually means.
- buffer pool management matters way more than i expected. going from direct disk I/O to a 1024-page cache was night and day.
- `fsync` vs `fdatasync` makes a real difference on linux. on windows it's less dramatic but still measurable.
- binary search in B+ tree nodes is a no-brainer optimization but i initially shipped with linear scan because "it's simpler" — lesson learned.

## project structure

```
src/
├── disk/
│   ├── page.rs          # page layout, CRC32
│   └── disk_manager.rs  # file I/O
├── btree/
│   ├── node.rs          # leaf + internal node ops
│   └── tree.rs          # B+ tree (insert/delete/split/scan)
├── buffer/
│   └── pool.rs          # buffer pool with CLOCK eviction
├── wal/
│   ├── log_record.rs    # WAL record format
│   └── wal_manager.rs   # WAL + recovery
├── txn/
│   ├── lock_manager.rs  # S2PL lock manager
│   └── txn_manager.rs   # transaction coordinator
├── engine.rs            # unified StorageEngine API
├── bin/cli.rs           # interactive CLI
└── main.rs              # demo program
```

## license

do whatever you want with it. it's a learning project.
