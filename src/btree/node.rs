use crate::disk::page::{Page, PageId, PageType, HEADER_SIZE, INVALID_PAGE_ID, PAGE_SIZE};

pub const MAX_KEY_SIZE: usize = 256;
pub const MAX_VALUE_SIZE: usize = 256;

const LEAF_SLOT_SIZE: usize = 6;
const INTERNAL_SLOT_SIZE: usize = 8;
const BTREE_HEADER_SIZE: usize = 10;

fn read_u16_le(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([data[off], data[off + 1]])
}

fn read_u32_le(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

fn write_u16_le(data: &mut [u8], off: usize, val: u16) {
    data[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

fn write_u32_le(data: &mut [u8], off: usize, val: u32) {
    data[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

pub struct BTreeNode {
    pub page: Page,
}

impl BTreeNode {
    pub fn from_page(page: Page) -> Self {
        BTreeNode { page }
    }

    pub fn new_leaf(page_id: PageId) -> Self {
        let mut page = Page::new(page_id);
        page.set_page_type(PageType::Leaf);
        page.set_num_slots(0);
        page.set_free_offset((HEADER_SIZE + BTREE_HEADER_SIZE) as u16);
        let mut node = BTreeNode { page };
        node.set_right_sibling(INVALID_PAGE_ID);
        node.set_parent(INVALID_PAGE_ID);
        node.set_level(0);
        node
    }

    pub fn new_internal(page_id: PageId) -> Self {
        let mut page = Page::new(page_id);
        page.set_page_type(PageType::Internal);
        page.set_num_slots(0);
        page.set_free_offset((HEADER_SIZE + BTREE_HEADER_SIZE) as u16);
        let mut node = BTreeNode { page };
        node.set_right_sibling(INVALID_PAGE_ID);
        node.set_parent(INVALID_PAGE_ID);
        node.set_level(1);
        node
    }

    pub fn is_leaf(&self) -> bool {
        self.page.get_page_type() == PageType::Leaf
    }

    fn btree_header_offset() -> usize {
        HEADER_SIZE
    }

    pub fn get_right_sibling(&self) -> PageId {
        let off = Self::btree_header_offset();
        read_u32_le(&self.page.data, off)
    }

    pub fn set_right_sibling(&mut self, pid: PageId) {
        let off = Self::btree_header_offset();
        write_u32_le(&mut self.page.data, off, pid);
        self.page.dirty = true;
    }

    pub fn get_parent(&self) -> PageId {
        let off = Self::btree_header_offset() + 4;
        read_u32_le(&self.page.data, off)
    }

    pub fn set_parent(&mut self, pid: PageId) {
        let off = Self::btree_header_offset() + 4;
        write_u32_le(&mut self.page.data, off, pid);
        self.page.dirty = true;
    }

    pub fn get_level(&self) -> u16 {
        let off = Self::btree_header_offset() + 8;
        read_u16_le(&self.page.data, off)
    }

    pub fn set_level(&mut self, level: u16) {
        let off = Self::btree_header_offset() + 8;
        write_u16_le(&mut self.page.data, off, level);
        self.page.dirty = true;
    }

    pub fn num_keys(&self) -> usize {
        self.page.get_num_slots() as usize
    }

    fn leaf_slots_start(&self) -> usize {
        HEADER_SIZE + BTREE_HEADER_SIZE
    }

    fn leaf_slot_offset(&self, idx: usize) -> usize {
        self.leaf_slots_start() + idx * LEAF_SLOT_SIZE
    }

    fn read_leaf_slot(&self, idx: usize) -> (u16, u16, u16) {
        let off = self.leaf_slot_offset(idx);
        let data = &self.page.data;
        let data_offset = u16::from_le_bytes([data[off], data[off + 1]]);
        let key_len = u16::from_le_bytes([data[off + 2], data[off + 3]]);
        let val_len = u16::from_le_bytes([data[off + 4], data[off + 5]]);
        (data_offset, key_len, val_len)
    }

    fn write_leaf_slot(&mut self, idx: usize, data_offset: u16, key_len: u16, val_len: u16) {
        let off = self.leaf_slot_offset(idx);
        self.page.data[off..off + 2].copy_from_slice(&data_offset.to_le_bytes());
        self.page.data[off + 2..off + 4].copy_from_slice(&key_len.to_le_bytes());
        self.page.data[off + 4..off + 6].copy_from_slice(&val_len.to_le_bytes());
        self.page.dirty = true;
    }

    pub fn leaf_key(&self, idx: usize) -> &[u8] {
        let (data_offset, key_len, _) = self.read_leaf_slot(idx);
        let start = data_offset as usize;
        &self.page.data[start..start + key_len as usize]
    }

    pub fn leaf_value(&self, idx: usize) -> &[u8] {
        let (data_offset, key_len, val_len) = self.read_leaf_slot(idx);
        let start = data_offset as usize + key_len as usize;
        &self.page.data[start..start + val_len as usize]
    }

    /// Linear scan to find key slot
    pub fn leaf_find_slot(&self, key: &[u8]) -> (usize, bool) {
        let n = self.num_keys();
        for i in 0..n {
            let k = self.leaf_key(i);
            match k.cmp(key) {
                std::cmp::Ordering::Equal => return (i, true),
                std::cmp::Ordering::Greater => return (i, false),
                _ => {}
            }
        }
        (n, false)
    }

    fn leaf_data_frontier(&self) -> usize {
        let n = self.num_keys();
        if n == 0 {
            return PAGE_SIZE;
        }
        let mut min = PAGE_SIZE;
        for i in 0..n {
            let (off, _, _) = self.read_leaf_slot(i);
            if (off as usize) < min {
                min = off as usize;
            }
        }
        min
    }

    fn leaf_available_space(&self) -> usize {
        let n = self.num_keys();
        let slots_end = self.leaf_slots_start() + (n + 1) * LEAF_SLOT_SIZE;
        let data_start = self.leaf_data_frontier();
        if data_start > slots_end {
            data_start - slots_end
        } else {
            0
        }
    }

    pub fn leaf_can_fit(&self, key: &[u8], value: &[u8]) -> bool {
        let needed = LEAF_SLOT_SIZE + key.len() + value.len();
        self.leaf_available_space() >= needed
    }

    pub fn leaf_insert(&mut self, key: &[u8], value: &[u8]) -> bool {
        let (slot_idx, found) = self.leaf_find_slot(key);

        if found {
            self.leaf_update(slot_idx, value);
            return false;
        }

        let n = self.num_keys();
        let current_data_start = self.leaf_data_frontier();
        let kv_len = key.len() + value.len();
        let new_data_offset = current_data_start - kv_len;

        self.page.data[new_data_offset..new_data_offset + key.len()].copy_from_slice(key);
        self.page.data[new_data_offset + key.len()..new_data_offset + kv_len]
            .copy_from_slice(value);

        for i in (slot_idx..n).rev() {
            let slot = self.read_leaf_slot(i);
            self.write_leaf_slot(i + 1, slot.0, slot.1, slot.2);
        }

        self.write_leaf_slot(
            slot_idx,
            new_data_offset as u16,
            key.len() as u16,
            value.len() as u16,
        );

        self.page.set_num_slots(n as u16 + 1);
        self.page.dirty = true;
        true
    }

    fn leaf_update(&mut self, idx: usize, new_value: &[u8]) {
        let n = self.num_keys();
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(n);

        for i in 0..n {
            let key = self.leaf_key(i).to_vec();
            let val = if i == idx {
                new_value.to_vec()
            } else {
                self.leaf_value(i).to_vec()
            };
            entries.push((key, val));
        }

        let right_sib = self.get_right_sibling();
        let parent = self.get_parent();
        let level = self.get_level();

        let page_id = self.page.id;
        self.page = Page::new(page_id);
        self.page.set_page_type(PageType::Leaf);
        self.page.set_num_slots(0);
        self.page
            .set_free_offset((HEADER_SIZE + BTREE_HEADER_SIZE) as u16);
        self.set_right_sibling(right_sib);
        self.set_parent(parent);
        self.set_level(level);

        for (k, v) in &entries {
            self.leaf_insert(k, v);
        }
    }

    pub fn leaf_delete(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let (idx, found) = self.leaf_find_slot(key);
        if !found {
            return None;
        }

        let old_val = self.leaf_value(idx).to_vec();

        let n = self.num_keys();
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(n - 1);
        for i in 0..n {
            if i == idx {
                continue;
            }
            entries.push((self.leaf_key(i).to_vec(), self.leaf_value(i).to_vec()));
        }

        let right_sib = self.get_right_sibling();
        let parent = self.get_parent();
        let level = self.get_level();

        let page_id = self.page.id;
        self.page = Page::new(page_id);
        self.page.set_page_type(PageType::Leaf);
        self.page.set_num_slots(0);
        self.page
            .set_free_offset((HEADER_SIZE + BTREE_HEADER_SIZE) as u16);
        self.set_right_sibling(right_sib);
        self.set_parent(parent);
        self.set_level(level);

        for (k, v) in &entries {
            self.leaf_insert(k, v);
        }

        Some(old_val)
    }

    pub fn leaf_entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let n = self.num_keys();
        let mut result = Vec::with_capacity(n);
        for i in 0..n {
            result.push((self.leaf_key(i).to_vec(), self.leaf_value(i).to_vec()));
        }
        result
    }

    // === INTERNAL NODE ===

    fn internal_leftmost_child_offset() -> usize {
        HEADER_SIZE + BTREE_HEADER_SIZE
    }

    pub fn get_leftmost_child(&self) -> PageId {
        let off = Self::internal_leftmost_child_offset();
        read_u32_le(&self.page.data, off)
    }

    pub fn set_leftmost_child(&mut self, pid: PageId) {
        let off = Self::internal_leftmost_child_offset();
        write_u32_le(&mut self.page.data, off, pid);
        self.page.dirty = true;
    }

    fn internal_slots_start(&self) -> usize {
        Self::internal_leftmost_child_offset() + 4
    }

    fn internal_slot_offset(&self, idx: usize) -> usize {
        self.internal_slots_start() + idx * INTERNAL_SLOT_SIZE
    }

    fn read_internal_slot(&self, idx: usize) -> (u16, u16, u32) {
        let off = self.internal_slot_offset(idx);
        let d = &self.page.data;
        let key_offset = u16::from_le_bytes([d[off], d[off + 1]]);
        let key_len = u16::from_le_bytes([d[off + 2], d[off + 3]]);
        let right_child = u32::from_le_bytes([d[off + 4], d[off + 5], d[off + 6], d[off + 7]]);
        (key_offset, key_len, right_child)
    }

    fn write_internal_slot(&mut self, idx: usize, key_offset: u16, key_len: u16, right_child: u32) {
        let off = self.internal_slot_offset(idx);
        self.page.data[off..off + 2].copy_from_slice(&key_offset.to_le_bytes());
        self.page.data[off + 2..off + 4].copy_from_slice(&key_len.to_le_bytes());
        self.page.data[off + 4..off + 8].copy_from_slice(&right_child.to_le_bytes());
        self.page.dirty = true;
    }

    pub fn internal_key(&self, idx: usize) -> &[u8] {
        let (key_offset, key_len, _) = self.read_internal_slot(idx);
        let start = key_offset as usize;
        &self.page.data[start..start + key_len as usize]
    }

    pub fn internal_child(&self, idx: usize) -> PageId {
        if idx == 0 {
            self.get_leftmost_child()
        } else {
            let (_, _, right_child) = self.read_internal_slot(idx - 1);
            right_child
        }
    }

    pub fn num_children(&self) -> usize {
        self.num_keys() + 1
    }

    /// Linear scan for child finding
    pub fn internal_find_child(&self, key: &[u8]) -> usize {
        let n = self.num_keys();
        for i in 0..n {
            if key < self.internal_key(i) {
                return i;
            }
        }
        n
    }

    fn internal_data_frontier(&self) -> usize {
        let n = self.num_keys();
        if n == 0 {
            return PAGE_SIZE;
        }
        let mut min_off = PAGE_SIZE;
        for i in 0..n {
            let (off, _, _) = self.read_internal_slot(i);
            if (off as usize) < min_off {
                min_off = off as usize;
            }
        }
        min_off
    }

    fn internal_available_space(&self) -> usize {
        let n = self.num_keys();
        let slots_end = self.internal_slots_start() + (n + 1) * INTERNAL_SLOT_SIZE;
        let data_start = self.internal_data_frontier();
        if data_start > slots_end {
            data_start - slots_end
        } else {
            0
        }
    }

    /// Linear scan for insertion point
    pub fn internal_insert(&mut self, key: &[u8], right_child: PageId) {
        let n = self.num_keys();

        let mut pos = n;
        for i in 0..n {
            if key < self.internal_key(i) {
                pos = i;
                break;
            }
        }

        let current_data_start = self.internal_data_frontier();
        let new_data_offset = current_data_start - key.len();
        self.page.data[new_data_offset..new_data_offset + key.len()].copy_from_slice(key);

        for i in (pos..n).rev() {
            let slot = self.read_internal_slot(i);
            self.write_internal_slot(i + 1, slot.0, slot.1, slot.2);
        }

        self.write_internal_slot(pos, new_data_offset as u16, key.len() as u16, right_child);
        self.page.set_num_slots(n as u16 + 1);
        self.page.dirty = true;
    }

    pub fn internal_entries(&self) -> (PageId, Vec<(Vec<u8>, PageId)>) {
        let leftmost = self.get_leftmost_child();
        let n = self.num_keys();
        let mut entries = Vec::with_capacity(n);
        for i in 0..n {
            let (_, _, right_child) = self.read_internal_slot(i);
            entries.push((self.internal_key(i).to_vec(), right_child));
        }
        (leftmost, entries)
    }

    pub fn internal_can_fit(&self, key: &[u8]) -> bool {
        let needed = INTERNAL_SLOT_SIZE + key.len();
        self.internal_available_space() >= needed
    }

    pub fn internal_rebuild(&mut self, leftmost: PageId, entries: &[(Vec<u8>, PageId)]) {
        let right_sib = self.get_right_sibling();
        let parent = self.get_parent();
        let level = self.get_level();
        let page_id = self.page.id;

        self.page = Page::new(page_id);
        self.page.set_page_type(PageType::Internal);
        self.page.set_num_slots(0);
        self.page
            .set_free_offset((HEADER_SIZE + BTREE_HEADER_SIZE) as u16);
        self.set_right_sibling(right_sib);
        self.set_parent(parent);
        self.set_level(level);
        self.set_leftmost_child(leftmost);

        for (k, child) in entries {
            self.internal_insert(k, *child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_leaf_insert_and_search() {
        let mut node = BTreeNode::new_leaf(0);
        assert!(node.leaf_insert(b"key1", b"val1"));
        assert!(node.leaf_insert(b"key3", b"val3"));
        assert!(node.leaf_insert(b"key2", b"val2"));

        assert_eq!(node.num_keys(), 3);
        assert_eq!(node.leaf_key(0), b"key1");
        assert_eq!(node.leaf_key(1), b"key2");
        assert_eq!(node.leaf_key(2), b"key3");

        assert_eq!(node.leaf_value(0), b"val1");
        assert_eq!(node.leaf_value(1), b"val2");
        assert_eq!(node.leaf_value(2), b"val3");

        let (idx, found) = node.leaf_find_slot(b"key2");
        assert!(found);
        assert_eq!(idx, 1);

        let (_, found) = node.leaf_find_slot(b"key4");
        assert!(!found);
    }

    #[test]
    fn test_leaf_update() {
        let mut node = BTreeNode::new_leaf(0);
        node.leaf_insert(b"key1", b"old_value");
        assert_eq!(node.leaf_value(0), b"old_value");

        let was_new = node.leaf_insert(b"key1", b"new_value");
        assert!(!was_new);
        assert_eq!(node.num_keys(), 1);
        assert_eq!(node.leaf_value(0), b"new_value");
    }

    #[test]
    fn test_leaf_delete() {
        let mut node = BTreeNode::new_leaf(0);
        node.leaf_insert(b"a", b"1");
        node.leaf_insert(b"b", b"2");
        node.leaf_insert(b"c", b"3");

        let old = node.leaf_delete(b"b");
        assert_eq!(old, Some(b"2".to_vec()));
        assert_eq!(node.num_keys(), 2);
        assert_eq!(node.leaf_key(0), b"a");
        assert_eq!(node.leaf_key(1), b"c");

        let not_found = node.leaf_delete(b"z");
        assert!(not_found.is_none());
    }

    #[test]
    fn test_internal_node() {
        let mut node = BTreeNode::new_internal(0);
        node.set_leftmost_child(100);
        node.internal_insert(b"mid", 101);
        node.internal_insert(b"high", 102);
        node.internal_insert(b"aaa", 99);

        assert_eq!(node.num_keys(), 3);
        assert_eq!(node.internal_key(0), b"aaa");
        assert_eq!(node.internal_key(1), b"high");
        assert_eq!(node.internal_key(2), b"mid");

        assert_eq!(node.internal_child(0), 100);
        assert_eq!(node.internal_child(1), 99);
        assert_eq!(node.internal_child(2), 102);
        assert_eq!(node.internal_child(3), 101);

        assert_eq!(node.internal_find_child(b"bbb"), 1);
        assert_eq!(node.internal_find_child(b"aaa"), 1);
        assert_eq!(node.internal_find_child(b"a"), 0);
        assert_eq!(node.internal_find_child(b"zzz"), 3);
    }

    #[test]
    fn test_leaf_capacity() {
        let mut node = BTreeNode::new_leaf(0);
        let mut count = 0;
        loop {
            let key = format!("key{:04}", count);
            let val = format!("val{:04}", count);
            if !node.leaf_can_fit(key.as_bytes(), val.as_bytes()) {
                break;
            }
            node.leaf_insert(key.as_bytes(), val.as_bytes());
            count += 1;
        }
        assert!(count > 100, "Expected >100 entries, got {}", count);
    }
}
