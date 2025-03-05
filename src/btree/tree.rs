use crate::disk::disk_manager::DiskManager;
use crate::disk::page::{Page, PageId, INVALID_PAGE_ID};
use crate::error::Result;

use super::node::BTreeNode;

/// A disk-backed B+ tree index.
pub struct BPlusTree {
    pub root_page_id: PageId,
    disk: DiskManager,
}

impl BPlusTree {
    pub fn create(mut disk: DiskManager) -> Result<Self> {
        let root_id = disk.allocate_page()?;
        let mut node = BTreeNode::new_leaf(root_id);
        node.page.update_checksum();
        disk.write_page(&node.page)?;
        Ok(BPlusTree {
            root_page_id: root_id,
            disk,
        })
    }

    pub fn open(disk: DiskManager, root_page_id: PageId) -> Self {
        BPlusTree { root_page_id, disk }
    }

    pub fn disk_manager(&self) -> &DiskManager {
        &self.disk
    }

    pub fn disk_manager_mut(&mut self) -> &mut DiskManager {
        &mut self.disk
    }

    fn read_node(&mut self, page_id: PageId) -> Result<BTreeNode> {
        let page = self.disk.read_page(page_id)?;
        Ok(BTreeNode::from_page(page))
    }

    fn write_node(&mut self, node: &mut BTreeNode) -> Result<()> {
        node.page.update_checksum();
        self.disk.write_page(&node.page)?;
        Ok(())
    }

    fn find_leaf(&mut self, key: &[u8]) -> Result<PageId> {
        let mut current_id = self.root_page_id;
        loop {
            let node = self.read_node(current_id)?;
            if node.is_leaf() {
                return Ok(current_id);
            }
            let child_idx = node.internal_find_child(key);
            current_id = node.internal_child(child_idx);
        }
    }

    pub fn search(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let leaf_id = self.find_leaf(key)?;
        let node = self.read_node(leaf_id)?;
        let (idx, found) = node.leaf_find_slot(key);
        if found {
            Ok(Some(node.leaf_value(idx).to_vec()))
        } else {
            Ok(None)
        }
    }

    pub fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<bool> {
        let leaf_id = self.find_leaf(key)?;
        let mut node = self.read_node(leaf_id)?;

        let (_, found) = node.leaf_find_slot(key);
        if found {
            node.leaf_insert(key, value);
            self.write_node(&mut node)?;
            return Ok(false);
        }

        if node.leaf_can_fit(key, value) {
            node.leaf_insert(key, value);
            self.write_node(&mut node)?;
            return Ok(true);
        }

        self.split_and_insert_leaf(node, key, value)?;
        Ok(true)
    }

    fn split_and_insert_leaf(
        &mut self,
        mut old_node: BTreeNode,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        let mut entries = old_node.leaf_entries();
        let pos = entries
            .binary_search_by(|(k, _)| k.as_slice().cmp(key))
            .unwrap_or_else(|i| i);
        entries.insert(pos, (key.to_vec(), value.to_vec()));

        let total = entries.len();
        let split_point = total / 2;

        let left_entries = &entries[..split_point];
        let right_entries = &entries[split_point..];

        let right_id = self.disk.allocate_page()?;
        let mut right_node = BTreeNode::new_leaf(right_id);

        right_node.set_right_sibling(old_node.get_right_sibling());
        old_node.set_right_sibling(right_id);

        let old_id = old_node.page.id;
        let parent_id = old_node.get_parent();

        old_node = BTreeNode::new_leaf(old_id);
        old_node.set_parent(parent_id);
        old_node.set_right_sibling(right_id);
        for (k, v) in left_entries {
            old_node.leaf_insert(k, v);
        }

        for (k, v) in right_entries {
            right_node.leaf_insert(k, v);
        }

        let split_key = right_node.leaf_key(0).to_vec();

        self.write_node(&mut old_node)?;
        self.write_node(&mut right_node)?;

        self.insert_into_parent(old_id, &split_key, right_id, parent_id)?;
        Ok(())
    }

    fn insert_into_parent(
        &mut self,
        left_child: PageId,
        key: &[u8],
        right_child: PageId,
        parent_id: PageId,
    ) -> Result<()> {
        if parent_id == INVALID_PAGE_ID {
            let new_root_id = self.disk.allocate_page()?;
            let mut new_root = BTreeNode::new_internal(new_root_id);
            new_root.set_leftmost_child(left_child);
            new_root.internal_insert(key, right_child);
            self.write_node(&mut new_root)?;

            let mut left = self.read_node(left_child)?;
            left.set_parent(new_root_id);
            self.write_node(&mut left)?;

            let mut right = self.read_node(right_child)?;
            right.set_parent(new_root_id);
            self.write_node(&mut right)?;

            self.root_page_id = new_root_id;
            return Ok(());
        }

        let mut parent = self.read_node(parent_id)?;

        if parent.internal_can_fit(key) {
            parent.internal_insert(key, right_child);
            self.write_node(&mut parent)?;

            let mut right = self.read_node(right_child)?;
            right.set_parent(parent_id);
            self.write_node(&mut right)?;

            return Ok(());
        }

        self.split_internal(parent, key, right_child)?;
        Ok(())
    }

    fn split_internal(
        &mut self,
        old_node: BTreeNode,
        new_key: &[u8],
        new_child: PageId,
    ) -> Result<()> {
        let (leftmost, mut entries) = old_node.internal_entries();
        let pos = entries
            .binary_search_by(|(k, _)| k.as_slice().cmp(new_key))
            .unwrap_or_else(|i| i);
        entries.insert(pos, (new_key.to_vec(), new_child));

        let total = entries.len();
        let split_point = total / 2;

        let left_entries = &entries[..split_point];
        let push_up_key = entries[split_point].0.clone();
        let right_leftmost = entries[split_point].1;
        let right_entries = &entries[split_point + 1..];

        let right_id = self.disk.allocate_page()?;
        let mut right_node = BTreeNode::new_internal(right_id);
        right_node.set_level(old_node.get_level());
        right_node.internal_rebuild(right_leftmost, right_entries);

        let old_id = old_node.page.id;
        let parent_id = old_node.get_parent();
        let level = old_node.get_level();

        let mut new_old_node = BTreeNode::new_internal(old_id);
        new_old_node.set_level(level);
        new_old_node.set_parent(parent_id);
        new_old_node.internal_rebuild(leftmost, left_entries);

        self.write_node(&mut new_old_node)?;
        self.write_node(&mut right_node)?;

        self.update_children_parent(right_id)?;

        self.insert_into_parent(old_id, &push_up_key, right_id, parent_id)?;
        Ok(())
    }

    fn update_children_parent(&mut self, parent_id: PageId) -> Result<()> {
        let parent = self.read_node(parent_id)?;
        let n = parent.num_children();
        for i in 0..n {
            let child_id = parent.internal_child(i);
            let mut child = self.read_node(child_id)?;
            child.set_parent(parent_id);
            self.write_node(&mut child)?;
        }
        Ok(())
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let leaf_id = self.find_leaf(key)?;
        let mut node = self.read_node(leaf_id)?;
        let result = node.leaf_delete(key);
        if result.is_some() {
            self.write_node(&mut node)?;
        }
        Ok(result)
    }

    pub fn scan_all(&mut self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut current_id = self.root_page_id;
        loop {
            let node = self.read_node(current_id)?;
            if node.is_leaf() {
                break;
            }
            current_id = node.get_leftmost_child();
        }

        let mut result = Vec::new();
        loop {
            let node = self.read_node(current_id)?;
            let entries = node.leaf_entries();
            result.extend(entries);

            let next = node.get_right_sibling();
            if next == INVALID_PAGE_ID {
                break;
            }
            current_id = next;
        }

        Ok(result)
    }

    pub fn range_scan(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let leaf_id = self.find_leaf(start)?;
        let mut current_id = leaf_id;
        let mut result = Vec::new();

        'outer: loop {
            let node = self.read_node(current_id)?;
            let n = node.num_keys();
            for i in 0..n {
                let key = node.leaf_key(i);
                if key > end {
                    break 'outer;
                }
                if key >= start {
                    result.push((key.to_vec(), node.leaf_value(i).to_vec()));
                }
            }

            let next = node.get_right_sibling();
            if next == INVALID_PAGE_ID {
                break;
            }
            current_id = next;
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let id = COUNTER.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!("storagedb_bt_test_{}", id));
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

    fn create_tree() -> (BPlusTree, TempDir) {
        let dir = TempDir::new();
        let path = dir.path().join("test.db");
        let dm = DiskManager::open(&path).unwrap();
        let tree = BPlusTree::create(dm).unwrap();
        (tree, dir)
    }

    #[test]
    fn test_insert_and_search() {
        let (mut tree, _dir) = create_tree();
        assert!(tree.insert(b"hello", b"world").unwrap());
        assert!(tree.insert(b"foo", b"bar").unwrap());
        assert!(tree.insert(b"rust", b"lang").unwrap());

        assert_eq!(tree.search(b"hello").unwrap(), Some(b"world".to_vec()));
        assert_eq!(tree.search(b"foo").unwrap(), Some(b"bar".to_vec()));
        assert_eq!(tree.search(b"rust").unwrap(), Some(b"lang".to_vec()));
        assert_eq!(tree.search(b"missing").unwrap(), None);
    }

    #[test]
    fn test_update() {
        let (mut tree, _dir) = create_tree();
        assert!(tree.insert(b"key", b"val1").unwrap());
        assert!(!tree.insert(b"key", b"val2").unwrap());
        assert_eq!(tree.search(b"key").unwrap(), Some(b"val2".to_vec()));
    }

    #[test]
    fn test_delete() {
        let (mut tree, _dir) = create_tree();
        tree.insert(b"a", b"1").unwrap();
        tree.insert(b"b", b"2").unwrap();
        tree.insert(b"c", b"3").unwrap();

        assert_eq!(tree.delete(b"b").unwrap(), Some(b"2".to_vec()));
        assert_eq!(tree.search(b"b").unwrap(), None);
        assert_eq!(tree.search(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(tree.search(b"c").unwrap(), Some(b"3".to_vec()));
        assert_eq!(tree.delete(b"missing").unwrap(), None);
    }

    #[test]
    fn test_leaf_split() {
        let (mut tree, _dir) = create_tree();
        for i in 0..500 {
            let key = format!("key{:05}", i);
            let val = format!("val{:05}", i);
            tree.insert(key.as_bytes(), val.as_bytes()).unwrap();
        }
        for i in 0..500 {
            let key = format!("key{:05}", i);
            let val = format!("val{:05}", i);
            let result = tree.search(key.as_bytes()).unwrap();
            assert_eq!(
                result,
                Some(val.as_bytes().to_vec()),
                "Failed at key {}",
                key
            );
        }
    }

    #[test]
    fn test_scan_all() {
        let (mut tree, _dir) = create_tree();
        tree.insert(b"c", b"3").unwrap();
        tree.insert(b"a", b"1").unwrap();
        tree.insert(b"b", b"2").unwrap();

        let all = tree.scan_all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], (b"a".to_vec(), b"1".to_vec()));
        assert_eq!(all[1], (b"b".to_vec(), b"2".to_vec()));
        assert_eq!(all[2], (b"c".to_vec(), b"3".to_vec()));
    }

    #[test]
    fn test_range_scan() {
        let (mut tree, _dir) = create_tree();
        for i in 0..100 {
            let key = format!("k{:03}", i);
            let val = format!("v{:03}", i);
            tree.insert(key.as_bytes(), val.as_bytes()).unwrap();
        }
        let range = tree.range_scan(b"k020", b"k030").unwrap();
        assert_eq!(range.len(), 11);
        assert_eq!(range[0].0, b"k020".to_vec());
        assert_eq!(range[10].0, b"k030".to_vec());
    }

    #[test]
    fn test_large_insert_and_delete() {
        let (mut tree, _dir) = create_tree();
        let count = 1000;
        for i in 0..count {
            let key = format!("k{:06}", i);
            let val = format!("v{:06}", i);
            tree.insert(key.as_bytes(), val.as_bytes()).unwrap();
        }
        for i in (0..count).step_by(2) {
            let key = format!("k{:06}", i);
            tree.delete(key.as_bytes()).unwrap();
        }
        for i in 0..count {
            let key = format!("k{:06}", i);
            let result = tree.search(key.as_bytes()).unwrap();
            if i % 2 == 0 {
                assert_eq!(result, None, "Should be deleted: {}", key);
            } else {
                let val = format!("v{:06}", i);
                assert_eq!(
                    result,
                    Some(val.as_bytes().to_vec()),
                    "Should exist: {}",
                    key
                );
            }
        }
    }
}
