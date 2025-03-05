use std::collections::{HashMap, VecDeque};

use crate::disk::disk_manager::DiskManager;
use crate::disk::page::{Page, PageId};
use crate::error::{Error, Result};

/// A frame in the buffer pool holds one cached page.
#[derive(Debug)]
struct BufferFrame {
    page: Page,
    pin_count: u32,
    dirty: bool,
    /// Reference bit for CLOCK eviction (approximates LRU)
    ref_bit: bool,
}

/// Buffer Pool Manager — caches disk pages in memory with LRU-Clock eviction.
///
/// Key invariant: A dirty page must have its WAL record flushed before the page
/// itself is written to disk (WAL protocol / write-ahead logging rule).
pub struct BufferPoolManager {
    /// Page frames in the pool
    frames: Vec<Option<BufferFrame>>,
    /// Maps page_id -> frame_index for O(1) lookup
    page_table: HashMap<PageId, usize>,
    /// Free frame indices
    free_list: VecDeque<usize>,
    /// Clock hand for eviction
    clock_hand: usize,
    /// Pool capacity
    capacity: usize,
    /// Underlying disk manager
    disk: DiskManager,
}

impl BufferPoolManager {
    /// Create a new buffer pool with the given capacity (number of pages).
    pub fn new(disk: DiskManager, capacity: usize) -> Self {
        let mut frames = Vec::with_capacity(capacity);
        let mut free_list = VecDeque::with_capacity(capacity);

        for i in 0..capacity {
            frames.push(None);
            free_list.push_back(i);
        }

        BufferPoolManager {
            frames,
            page_table: HashMap::with_capacity(capacity),
            free_list,
            clock_hand: 0,
            capacity,
            disk,
        }
    }

    /// Fetch a page from the pool. If not cached, read from disk.
    /// Pins the page (increments pin count).
    pub fn fetch_page(&mut self, page_id: PageId) -> Result<&Page> {
        // Check if page is already in the pool
        if let Some(&frame_idx) = self.page_table.get(&page_id) {
            let frame = self.frames[frame_idx].as_mut().unwrap();
            frame.pin_count += 1;
            frame.ref_bit = true;
            return Ok(&self.frames[frame_idx].as_ref().unwrap().page);
        }

        // Need to bring page in from disk
        let frame_idx = self.get_free_frame()?;
        let page = self.disk.read_page(page_id)?;

        self.frames[frame_idx] = Some(BufferFrame {
            page,
            pin_count: 1,
            dirty: false,
            ref_bit: true,
        });
        self.page_table.insert(page_id, frame_idx);

        Ok(&self.frames[frame_idx].as_ref().unwrap().page)
    }

    /// Fetch a mutable reference to a page. Marks it dirty.
    pub fn fetch_page_mut(&mut self, page_id: PageId) -> Result<&mut Page> {
        // Ensure page is in the pool
        if !self.page_table.contains_key(&page_id) {
            // Load it first
            let frame_idx = self.get_free_frame()?;
            let page = self.disk.read_page(page_id)?;
            self.frames[frame_idx] = Some(BufferFrame {
                page,
                pin_count: 0,
                dirty: false,
                ref_bit: true,
            });
            self.page_table.insert(page_id, frame_idx);
        }

        let &frame_idx = self.page_table.get(&page_id).unwrap();
        let frame = self.frames[frame_idx].as_mut().unwrap();
        frame.pin_count += 1;
        frame.dirty = true;
        frame.ref_bit = true;

        Ok(&mut self.frames[frame_idx].as_mut().unwrap().page)
    }

    /// Unpin a page (decrement pin count). Set dirty if the page was modified.
    pub fn unpin_page(&mut self, page_id: PageId, dirty: bool) -> Result<()> {
        if let Some(&frame_idx) = self.page_table.get(&page_id) {
            let frame = self.frames[frame_idx].as_mut().unwrap();
            if frame.pin_count > 0 {
                frame.pin_count -= 1;
            }
            if dirty {
                frame.dirty = true;
            }
            Ok(())
        } else {
            Err(Error::PageNotFound(page_id))
        }
    }

    /// Allocate a new page through the buffer pool
    pub fn new_page(&mut self) -> Result<PageId> {
        let page_id = self.disk.allocate_page()?;

        let frame_idx = self.get_free_frame()?;
        let page = Page::new(page_id);

        self.frames[frame_idx] = Some(BufferFrame {
            page,
            pin_count: 1,
            dirty: true,
            ref_bit: true,
        });
        self.page_table.insert(page_id, frame_idx);

        Ok(page_id)
    }

    /// Flush a specific page to disk if it's dirty
    pub fn flush_page(&mut self, page_id: PageId) -> Result<()> {
        if let Some(&frame_idx) = self.page_table.get(&page_id) {
            let frame = self.frames[frame_idx].as_mut().unwrap();
            if frame.dirty {
                self.disk.write_page(&frame.page)?;
                frame.dirty = false;
            }
            Ok(())
        } else {
            Ok(()) // Not in pool, nothing to flush
        }
    }

    /// Flush all dirty pages to disk
    pub fn flush_all(&mut self) -> Result<()> {
        for i in 0..self.capacity {
            if let Some(frame) = &self.frames[i] {
                if frame.dirty {
                    self.disk.write_page(&frame.page)?;
                }
            }
            if let Some(frame) = &mut self.frames[i] {
                frame.dirty = false;
            }
        }
        self.disk.sync()?;
        Ok(())
    }

    /// Get a free frame, evicting a page if necessary using CLOCK algorithm.
    fn get_free_frame(&mut self) -> Result<usize> {
        // Try the free list first
        if let Some(idx) = self.free_list.pop_front() {
            return Ok(idx);
        }

        // CLOCK eviction: scan for an unpinned page with ref_bit = false
        let start = self.clock_hand;
        loop {
            if let Some(frame) = &self.frames[self.clock_hand] {
                if frame.pin_count == 0 {
                    if !frame.ref_bit {
                        // Evict this frame
                        let evicted_page_id = frame.page.id;

                        // Flush if dirty
                        if frame.dirty {
                            self.disk.write_page(&frame.page)?;
                        }

                        self.page_table.remove(&evicted_page_id);
                        let idx = self.clock_hand;
                        self.frames[idx] = None;
                        self.clock_hand = (self.clock_hand + 1) % self.capacity;
                        return Ok(idx);
                    } else {
                        // Clear the ref bit (second chance)
                        self.frames[self.clock_hand].as_mut().unwrap().ref_bit = false;
                    }
                }
            }

            self.clock_hand = (self.clock_hand + 1) % self.capacity;

            // If we've gone all the way around twice without finding a victim
            if self.clock_hand == start {
                // One more pass — all ref bits should now be cleared
                for _ in 0..self.capacity {
                    if let Some(frame) = &self.frames[self.clock_hand] {
                        if frame.pin_count == 0 {
                            let evicted_page_id = frame.page.id;
                            if frame.dirty {
                                self.disk.write_page(&frame.page)?;
                            }
                            self.page_table.remove(&evicted_page_id);
                            let idx = self.clock_hand;
                            self.frames[idx] = None;
                            self.clock_hand = (self.clock_hand + 1) % self.capacity;
                            return Ok(idx);
                        }
                    }
                    self.clock_hand = (self.clock_hand + 1) % self.capacity;
                }
                return Err(Error::BufferPoolFull);
            }
        }
    }

    /// Get pool statistics
    pub fn stats(&self) -> BufferPoolStats {
        let mut pinned = 0;
        let mut dirty = 0;
        let mut occupied = 0;

        for frame in &self.frames {
            if let Some(f) = frame {
                occupied += 1;
                if f.pin_count > 0 {
                    pinned += 1;
                }
                if f.dirty {
                    dirty += 1;
                }
            }
        }

        BufferPoolStats {
            capacity: self.capacity,
            occupied,
            pinned,
            dirty,
            free: self.free_list.len(),
        }
    }

    /// Get a reference to the underlying disk manager
    pub fn disk_manager(&self) -> &DiskManager {
        &self.disk
    }

    /// Get a mutable reference to the underlying disk manager
    pub fn disk_manager_mut(&mut self) -> &mut DiskManager {
        &mut self.disk
    }
}

#[derive(Debug)]
pub struct BufferPoolStats {
    pub capacity: usize,
    pub occupied: usize,
    pub pinned: usize,
    pub dirty: usize,
    pub free: usize,
}

impl std::fmt::Display for BufferPoolStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BufferPool[capacity={}, occupied={}, pinned={}, dirty={}, free={}]",
            self.capacity, self.occupied, self.pinned, self.dirty, self.free
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::page::PageType;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let id = COUNTER.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!("storagedb_bp_test_{}", id));
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

    fn create_pool(capacity: usize) -> (BufferPoolManager, TempDir) {
        let dir = TempDir::new();
        let path = dir.path().join("test.db");
        let dm = DiskManager::open(&path).unwrap();
        (BufferPoolManager::new(dm, capacity), dir)
    }

    #[test]
    fn test_new_page_and_fetch() {
        let (mut pool, _dir) = create_pool(10);

        let pid = pool.new_page().unwrap();

        {
            let page = pool.fetch_page_mut(pid).unwrap();
            page.set_page_type(PageType::Leaf);
            page.data_region_mut()[0..5].copy_from_slice(b"hello");
        }
        pool.unpin_page(pid, true).unwrap();
        pool.unpin_page(pid, false).unwrap(); // from new_page pin

        pool.flush_page(pid).unwrap();

        // Read it back
        let page = pool.fetch_page(pid).unwrap();
        assert_eq!(page.get_page_type(), PageType::Leaf);
        assert_eq!(&page.data_region()[0..5], b"hello");
    }

    #[test]
    fn test_eviction() {
        let (mut pool, _dir) = create_pool(3);

        // Allocate 3 pages, filling the pool
        let p0 = pool.new_page().unwrap();
        let p1 = pool.new_page().unwrap();
        let p2 = pool.new_page().unwrap();

        // Unpin all
        pool.unpin_page(p0, false).unwrap();
        pool.unpin_page(p1, false).unwrap();
        pool.unpin_page(p2, false).unwrap();

        // Allocate a 4th — should evict one
        let p3 = pool.new_page().unwrap();
        pool.unpin_page(p3, false).unwrap();

        let stats = pool.stats();
        assert_eq!(stats.occupied, 3);
    }

    #[test]
    fn test_flush_all() {
        let (mut pool, _dir) = create_pool(5);

        for _ in 0..5 {
            let pid = pool.new_page().unwrap();
            {
                let page = pool.fetch_page_mut(pid).unwrap();
                page.data_region_mut()[0] = 0xAA;
            }
            pool.unpin_page(pid, true).unwrap();
            pool.unpin_page(pid, false).unwrap();
        }

        let stats = pool.stats();
        assert_eq!(stats.dirty, 5);

        pool.flush_all().unwrap();

        let stats = pool.stats();
        assert_eq!(stats.dirty, 0);
    }

    #[test]
    fn test_pinned_page_not_evicted() {
        let (mut pool, _dir) = create_pool(2);

        let _p0 = pool.new_page().unwrap();
        let p1 = pool.new_page().unwrap();
        // p0 still pinned (pin_count=1 from new_page), p1 unpinned
        pool.unpin_page(p1, false).unwrap();

        // Allocate a 3rd page — should evict p1 (unpinned), not p0 (pinned)
        let p2 = pool.new_page().unwrap();
        pool.unpin_page(p2, false).unwrap();

        let stats = pool.stats();
        assert_eq!(stats.pinned, 1); // p0 is still pinned
    }
}
