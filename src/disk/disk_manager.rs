use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use super::page::{Page, PageId, PAGE_SIZE};
use crate::error::Result;

/// Manages reading/writing pages to/from a database file on disk.
///
/// The file is organized as a sequence of fixed-size pages.
/// Page 0 is reserved for metadata.
pub struct DiskManager {
    file: File,
    num_pages: AtomicU32,
    file_path: String,
}

impl DiskManager {
    /// Open or create a database file
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_str = path.as_ref().to_string_lossy().to_string();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        let file_len = file.metadata()?.len();
        let num_pages = if file_len == 0 {
            0
        } else {
            (file_len / PAGE_SIZE as u64) as u32
        };

        Ok(DiskManager {
            file,
            num_pages: AtomicU32::new(num_pages),
            file_path: path_str,
        })
    }

    /// Read a page from disk
    pub fn read_page(&mut self, page_id: PageId) -> Result<Page> {
        let offset = page_id as u64 * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;

        let mut buf = [0u8; PAGE_SIZE];
        self.file.read_exact(&mut buf)?;

        Ok(Page::from_bytes(page_id, &buf))
    }

    /// Write a page to disk.
    /// Note: Does NOT flush/sync — callers must explicitly call sync() for durability.
    /// This avoids catastrophic throughput loss from flushing on every single write.
    pub fn write_page(&mut self, page: &Page) -> Result<()> {
        let offset = page.id as u64 * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&page.data)?;
        Ok(())
    }

    /// Allocate a new page and return its ID.
    /// Extends the file by seeking to the new position (sparse file) rather than
    /// writing a zeroed page — avoids an unnecessary disk write on every allocation.
    pub fn allocate_page(&mut self) -> Result<PageId> {
        let page_id = self.num_pages.fetch_add(1, Ordering::SeqCst);
        // Extend file by writing a single byte at the end of the new page region.
        // This is sufficient to extend the file; the OS will zero-fill the gap.
        let end_offset = (page_id as u64 + 1) * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(end_offset - 1))?;
        self.file.write_all(&[0u8])?;
        Ok(page_id)
    }

    /// Get the total number of pages
    pub fn num_pages(&self) -> u32 {
        self.num_pages.load(Ordering::SeqCst)
    }

    /// Force sync data to disk. Uses sync_data() (fdatasync) to avoid
    /// flushing unnecessary file metadata.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Get the file path
    pub fn path(&self) -> &str {
        &self.file_path
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
            let p = std::env::temp_dir().join(format!("storagedb_dm_test_{}", id));
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
    fn test_disk_manager_read_write() {
        let dir = TempDir::new();
        let db_path = dir.path().join("test.db");

        let mut dm = DiskManager::open(&db_path).unwrap();

        // Allocate and write
        let pid = dm.allocate_page().unwrap();
        assert_eq!(pid, 0);

        let mut page = Page::new(pid);
        page.set_page_type(PageType::Leaf);
        page.data_region_mut()[0..6].copy_from_slice(b"foobar");
        page.update_checksum();
        dm.write_page(&page).unwrap();

        // Read back
        let read_page = dm.read_page(pid).unwrap();
        assert_eq!(read_page.get_page_type(), PageType::Leaf);
        assert_eq!(&read_page.data_region()[0..6], b"foobar");
        assert!(read_page.verify_checksum());
    }

    #[test]
    fn test_multiple_pages() {
        let dir = TempDir::new();
        let db_path = dir.path().join("multi.db");
        let mut dm = DiskManager::open(&db_path).unwrap();

        for i in 0..10 {
            let pid = dm.allocate_page().unwrap();
            assert_eq!(pid, i);
            let mut page = Page::new(pid);
            page.set_page_type(PageType::Leaf);
            let val = format!("page-{}", i);
            page.data_region_mut()[..val.len()].copy_from_slice(val.as_bytes());
            page.update_checksum();
            dm.write_page(&page).unwrap();
        }

        assert_eq!(dm.num_pages(), 10);

        // Verify all pages
        for i in 0..10 {
            let page = dm.read_page(i).unwrap();
            let expected = format!("page-{}", i);
            assert_eq!(&page.data_region()[..expected.len()], expected.as_bytes());
            assert!(page.verify_checksum());
        }
    }
}
