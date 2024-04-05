use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use super::page::{Page, PageId, PAGE_SIZE};
use crate::error::Result;

/// Manages reading/writing pages to/from a database file on disk.
pub struct DiskManager {
    file: File,
    num_pages: AtomicU32,
    file_path: String,
}

impl DiskManager {
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

    pub fn read_page(&mut self, page_id: PageId) -> Result<Page> {
        let offset = page_id as u64 * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;

        let mut buf = [0u8; PAGE_SIZE];
        self.file.read_exact(&mut buf)?;

        Ok(Page::from_bytes(page_id, &buf))
    }

    pub fn write_page(&mut self, page: &Page) -> Result<()> {
        let offset = page.id as u64 * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&page.data)?;
        self.file.flush()?;
        Ok(())
    }

    pub fn allocate_page(&mut self) -> Result<PageId> {
        let page_id = self.num_pages.fetch_add(1, Ordering::SeqCst);
        let page = Page::new(page_id);
        self.write_page(&page)?;
        Ok(page_id)
    }

    pub fn num_pages(&self) -> u32 {
        self.num_pages.load(Ordering::SeqCst)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

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

        let pid = dm.allocate_page().unwrap();
        assert_eq!(pid, 0);

        let mut page = Page::new(pid);
        page.set_page_type(PageType::Leaf);
        page.data_region_mut()[0..6].copy_from_slice(b"foobar");
        page.update_checksum();
        dm.write_page(&page).unwrap();

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

        for i in 0..10 {
            let page = dm.read_page(i).unwrap();
            let expected = format!("page-{}", i);
            assert_eq!(&page.data_region()[..expected.len()], expected.as_bytes());
            assert!(page.verify_checksum());
        }
    }
}
