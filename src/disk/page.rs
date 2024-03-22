/// Page size: 4KB (standard for most databases)
pub const PAGE_SIZE: usize = 4096;

/// Page header: page_id(4) + page_type(1) + num_slots(2) + free_space_offset(2) + checksum(4) = 13 bytes
pub const HEADER_SIZE: usize = 16; // aligned to 16 bytes

pub type PageId = u32;

pub const INVALID_PAGE_ID: PageId = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    Invalid = 0,
    Meta = 1,
    Internal = 2, // B+ tree internal node
    Leaf = 3,     // B+ tree leaf node
    Overflow = 4, // overflow pages for large values
    FreeList = 5, // free page tracking
}

impl From<u8> for PageType {
    fn from(v: u8) -> Self {
        match v {
            1 => PageType::Meta,
            2 => PageType::Internal,
            3 => PageType::Leaf,
            4 => PageType::Overflow,
            5 => PageType::FreeList,
            _ => PageType::Invalid,
        }
    }
}

/// CRC32 lookup table (IEEE / CRC-32b polynomial 0xEDB88320)
/// This is a slicing-by-4 table: 4 tables of 256 entries each.
/// Processes 4 bytes per iteration for ~4x speedup over byte-at-a-time.
const CRC32_TABLES: [[u32; 256]; 4] = {
    // First, build the standard byte-at-a-time table
    let mut t0 = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        t0[i as usize] = crc;
        i += 1;
    }

    // Build the slicing tables: t1[i] = t0[(t0[i] >> 8) & 0xFF] ^ (t0[i] >> 8), etc.
    let mut t1 = [0u32; 256];
    let mut t2 = [0u32; 256];
    let mut t3 = [0u32; 256];

    let mut i = 0;
    while i < 256 {
        t1[i] = (t0[i] >> 8) ^ t0[(t0[i] & 0xFF) as usize];
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        t2[i] = (t1[i] >> 8) ^ t0[(t1[i] & 0xFF) as usize];
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        t3[i] = (t2[i] >> 8) ^ t0[(t2[i] & 0xFF) as usize];
        i += 1;
    }

    [t0, t1, t2, t3]
};

/// Compute CRC32 checksum using slicing-by-4 algorithm.
/// Processes 4 bytes per loop iteration (~4x faster than byte-at-a-time).
/// Falls back to byte-at-a-time for trailing bytes.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    let len = data.len();
    let mut i = 0;

    // Process 4 bytes at a time
    let chunks = len / 4;
    for _ in 0..chunks {
        let d = crc ^ u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        crc = CRC32_TABLES[3][(d & 0xFF) as usize]
            ^ CRC32_TABLES[2][((d >> 8) & 0xFF) as usize]
            ^ CRC32_TABLES[1][((d >> 16) & 0xFF) as usize]
            ^ CRC32_TABLES[0][((d >> 24) & 0xFF) as usize];
        i += 4;
    }

    // Process remaining bytes one at a time
    while i < len {
        let idx = ((crc ^ data[i] as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLES[0][idx];
        i += 1;
    }

    crc ^ 0xFFFFFFFF
}

/// Fixed-size page — the fundamental unit of storage.
///
/// Layout:
/// ```text
/// [0..4)    page_id      : u32
/// [4..5)    page_type    : u8
/// [5..7)    num_slots    : u16  (number of key-value slots)
/// [7..9)    free_offset  : u16  (offset of free space start)
/// [9..13)   checksum     : u32  (CRC32 of data region)
/// [13..16)  reserved     : 3 bytes padding
/// [16..4096) data        : payload
/// ```
#[derive(Clone)]
pub struct Page {
    pub id: PageId,
    pub data: [u8; PAGE_SIZE],
    pub dirty: bool,
    pub pin_count: u32,
}

impl Page {
    /// Create a new empty page
    pub fn new(id: PageId) -> Self {
        let mut page = Page {
            id,
            data: [0u8; PAGE_SIZE],
            dirty: false,
            pin_count: 0,
        };
        page.set_page_id(id);
        page.set_page_type(PageType::Invalid);
        page.set_num_slots(0);
        page.set_free_offset(HEADER_SIZE as u16);
        page
    }

    /// Create a page from raw bytes read from disk
    pub fn from_bytes(id: PageId, bytes: &[u8; PAGE_SIZE]) -> Self {
        let mut data = [0u8; PAGE_SIZE];
        data.copy_from_slice(bytes);
        Page {
            id,
            data,
            dirty: false,
            pin_count: 0,
        }
    }

    // --- Header accessors ---

    pub fn get_page_id(&self) -> PageId {
        u32::from_le_bytes([self.data[0], self.data[1], self.data[2], self.data[3]])
    }

    pub fn set_page_id(&mut self, id: PageId) {
        self.data[0..4].copy_from_slice(&id.to_le_bytes());
    }

    pub fn get_page_type(&self) -> PageType {
        PageType::from(self.data[4])
    }

    pub fn set_page_type(&mut self, pt: PageType) {
        self.data[4] = pt as u8;
        self.dirty = true;
    }

    pub fn get_num_slots(&self) -> u16 {
        u16::from_le_bytes([self.data[5], self.data[6]])
    }

    pub fn set_num_slots(&mut self, n: u16) {
        self.data[5..7].copy_from_slice(&n.to_le_bytes());
        self.dirty = true;
    }

    pub fn get_free_offset(&self) -> u16 {
        u16::from_le_bytes([self.data[7], self.data[8]])
    }

    pub fn set_free_offset(&mut self, offset: u16) {
        self.data[7..9].copy_from_slice(&offset.to_le_bytes());
        self.dirty = true;
    }

    pub fn get_checksum(&self) -> u32 {
        u32::from_le_bytes([self.data[9], self.data[10], self.data[11], self.data[12]])
    }

    pub fn set_checksum(&mut self, csum: u32) {
        self.data[9..13].copy_from_slice(&csum.to_le_bytes());
    }

    /// Compute CRC32 of the data region (after header)
    pub fn compute_checksum(&self) -> u32 {
        crc32(&self.data[HEADER_SIZE..])
    }

    /// Write checksum into header
    pub fn update_checksum(&mut self) {
        let csum = self.compute_checksum();
        self.set_checksum(csum);
    }

    /// Verify page integrity
    pub fn verify_checksum(&self) -> bool {
        self.get_checksum() == self.compute_checksum()
    }

    /// Get a slice of the data region (after header)
    pub fn data_region(&self) -> &[u8] {
        &self.data[HEADER_SIZE..]
    }

    /// Get a mutable slice of the data region
    pub fn data_region_mut(&mut self) -> &mut [u8] {
        &mut self.data[HEADER_SIZE..]
    }

    /// Available free space in the data region
    pub fn free_space(&self) -> usize {
        PAGE_SIZE - self.get_free_offset() as usize
    }
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("id", &self.id)
            .field("type", &self.get_page_type())
            .field("num_slots", &self.get_num_slots())
            .field("free_offset", &self.get_free_offset())
            .field("dirty", &self.dirty)
            .field("pin_count", &self.pin_count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_creation() {
        let page = Page::new(42);
        assert_eq!(page.get_page_id(), 42);
        assert_eq!(page.get_page_type(), PageType::Invalid);
        assert_eq!(page.get_num_slots(), 0);
        assert_eq!(page.get_free_offset(), HEADER_SIZE as u16);
    }

    #[test]
    fn test_page_type_roundtrip() {
        let mut page = Page::new(0);
        page.set_page_type(PageType::Leaf);
        assert_eq!(page.get_page_type(), PageType::Leaf);
        page.set_page_type(PageType::Internal);
        assert_eq!(page.get_page_type(), PageType::Internal);
    }

    #[test]
    fn test_checksum() {
        let mut page = Page::new(1);
        page.data_region_mut()[0..5].copy_from_slice(b"hello");
        page.update_checksum();
        assert!(page.verify_checksum());

        // corrupt data
        page.data_region_mut()[0] = 0xFF;
        assert!(!page.verify_checksum());
    }

    #[test]
    fn test_from_bytes() {
        let mut original = Page::new(7);
        original.set_page_type(PageType::Leaf);
        original.set_num_slots(3);
        original.data_region_mut()[0..4].copy_from_slice(&[1, 2, 3, 4]);
        original.update_checksum();

        let restored = Page::from_bytes(7, &original.data);
        assert_eq!(restored.get_page_id(), 7);
        assert_eq!(restored.get_page_type(), PageType::Leaf);
        assert_eq!(restored.get_num_slots(), 3);
        assert!(restored.verify_checksum());
    }

    #[test]
    fn test_crc32_known_value() {
        // "123456789" should produce CRC32 = 0xCBF43926
        let data = b"123456789";
        assert_eq!(crc32(data), 0xCBF43926);
    }
}
