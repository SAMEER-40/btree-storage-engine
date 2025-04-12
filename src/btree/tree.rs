use crate::buffer::BufferPoolManager;
use crate::disk::page::{PageId, INVALID_PAGE_ID};
use crate::error::Result;

use super::node::BTreeNode;

/// A disk-backed B+ tree index.
///
/// Uses the BufferPoolManager for all page I/O instead of hitting disk directly.
/// This means frequently-accessed pages (like the root and upper internal nodes)
/// stay cached in memory, dramatically reducing disk I/O.
///
/// Operations:
/// - `insert(pool, key, value)` — insert or update
/// - `search(pool, key)` -> Option<value>
/// - `delete(pool, key)` -> Option<old_value>
/// - `range_scan(pool, start, end)` -> Vec<(key, value)>
pub struct BPlusTree {
    pub root_page_id: PageId,
}

impl BPlusTree {
    /// Create a new B+ tree, allocating a root leaf page via the buffer pool.
    pub fn create(pool: &mut BufferPoolManager) -> Result<Self> {
        let root_id = pool.new_page()?;
        {
            let page = pool.fetch_page_mut(root_id)?;
            let mut node = BTreeNode::new_leaf(root_id);
            node.page.update_checksum();
            page.data.copy_from_slice(&node.page.data);
            page.dirty = true;
        }
        pool.unpin_page(root_id, true)?;
        // Unpin the new_page pin too
        pool.unpin_page(root_id, false)?;

        Ok(BPlusTree {
            root_page_id: root_id,
        })
    }

    /// Open an existing B+ tree with the given root page.
    pub fn open(root_page_id: PageId) -> Self {
        BPlusTree { root_page_id }
    }

    /// Read a node from the buffer pool. Caller must unpin when done.
    fn read_node(&self, pool: &mut BufferPoolManager, page_id: PageId) -> Result<BTreeNode> {
        let page = pool.fetch_page(page_id)?;
        let node = BTreeNode::from_page(page.clone());
        pool.unpin_page(page_id, false)?;
        Ok(node)
    }

    /// Write a node back through the buffer pool.
    fn write_node(&self, pool: &mut BufferPoolManager, node: &mut BTreeNode) -> Result<()> {
        node.page.update_checksum();
        let page_id = node.page.id;
        let page = pool.fetch_page_mut(page_id)?;
        page.data.copy_from_slice(&node.page.data);
        page.dirty = true;
        pool.unpin_page(page_id, true)?;
        Ok(())
    }

    /// Allocate a new page through the buffer pool.
    fn allocate_page(&self, pool: &mut BufferPoolManager) -> Result<PageId> {
        let page_id = pool.new_page()?;
        pool.unpin_page(page_id, false)?;
        Ok(page_id)
    }

    /// Find the leaf node that should contain the given key.
    fn find_leaf(&self, pool: &mut BufferPoolManager, key: &[u8]) -> Result<PageId> {
        let mut current_id = self.root_page_id;

        loop {
            let node = self.read_node(pool, current_id)?;
            if node.is_leaf() {
                return Ok(current_id);
            }

            let child_idx = node.internal_find_child(key);
            current_id = node.internal_child(child_idx);
        }
    }

    /// Search for a key, returning its value if found.
    pub fn search(&self, pool: &mut BufferPoolManager, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let leaf_id = self.find_leaf(pool, key)?;
        let node = self.read_node(pool, leaf_id)?;

        let (idx, found) = node.leaf_find_slot(key);
        if found {
            Ok(Some(node.leaf_value(idx).to_vec()))
        } else {
            Ok(None)
        }
    }

    /// Insert a key-value pair. Returns Ok(true) if new, Ok(false) if updated.
    pub fn insert(
        &mut self,
        pool: &mut BufferPoolManager,
        key: &[u8],
        value: &[u8],
    ) -> Result<bool> {
        let leaf_id = self.find_leaf(pool, key)?;
        let mut node = self.read_node(pool, leaf_id)?;

        // Check if key already exists
        let (_, found) = node.leaf_find_slot(key);
        if found {
            node.leaf_insert(key, value);
            self.write_node(pool, &mut node)?;
            return Ok(false);
        }

        // Check if the leaf has room
        if node.leaf_can_fit(key, value) {
            node.leaf_insert(key, value);
            self.write_node(pool, &mut node)?;
            return Ok(true);
        }

        // Need to split
        self.split_and_insert_leaf(pool, node, key, value)?;
        Ok(true)
    }

    /// Split a leaf node and insert the new key-value pair.
    fn split_and_insert_leaf(
        &mut self,
        pool: &mut BufferPoolManager,
        mut old_node: BTreeNode,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        // Collect all entries + the new one
        let mut entries = old_node.leaf_entries();

        // Find insertion position
        let pos = entries
            .binary_search_by(|(k, _)| k.as_slice().cmp(key))
            .unwrap_or_else(|i| i);
        entries.insert(pos, (key.to_vec(), value.to_vec()));

        let total = entries.len();
        let split_point = total / 2;

        // Left half stays in old node
        let left_entries = &entries[..split_point];
        let right_entries = &entries[split_point..];

        // Create new right node
        let right_id = self.allocate_page(pool)?;
        let mut right_node = BTreeNode::new_leaf(right_id);

        // Set sibling pointers
        right_node.set_right_sibling(old_node.get_right_sibling());
        old_node.set_right_sibling(right_id);

        // Rebuild old node with left entries
        let old_id = old_node.page.id;
        let parent_id = old_node.get_parent();

        // Clear and rebuild old node
        old_node = BTreeNode::new_leaf(old_id);
        old_node.set_parent(parent_id);
        old_node.set_right_sibling(right_id);
        for (k, v) in left_entries {
            old_node.leaf_insert(k, v);
        }

        // Fill right node
        for (k, v) in right_entries {
            right_node.leaf_insert(k, v);
        }

        // The split key is the first key of the right node
        let split_key = right_node.leaf_key(0).to_vec();

        // Write both nodes
        self.write_node(pool, &mut old_node)?;
        self.write_node(pool, &mut right_node)?;

        // Insert split key into parent
        self.insert_into_parent(pool, old_id, &split_key, right_id, parent_id)?;

        Ok(())
    }

    /// Insert a split key into the parent internal node.
    /// If the parent doesn't exist (root was split), create a new root.
    fn insert_into_parent(
        &mut self,
        pool: &mut BufferPoolManager,
        left_child: PageId,
        key: &[u8],
        right_child: PageId,
        parent_id: PageId,
    ) -> Result<()> {
        if parent_id == INVALID_PAGE_ID {
            // Root was split — create new root
            let new_root_id = self.allocate_page(pool)?;
            let mut new_root = BTreeNode::new_internal(new_root_id);
            new_root.set_leftmost_child(left_child);
            new_root.internal_insert(key, right_child);
            self.write_node(pool, &mut new_root)?;

            // Update children's parent pointers
            let mut left = self.read_node(pool, left_child)?;
            left.set_parent(new_root_id);
            self.write_node(pool, &mut left)?;

            let mut right = self.read_node(pool, right_child)?;
            right.set_parent(new_root_id);
            self.write_node(pool, &mut right)?;

            self.root_page_id = new_root_id;
            return Ok(());
        }

        let mut parent = self.read_node(pool, parent_id)?;

        if parent.internal_can_fit(key) {
            parent.internal_insert(key, right_child);
            self.write_node(pool, &mut parent)?;

            // Update right child's parent
            let mut right = self.read_node(pool, right_child)?;
            right.set_parent(parent_id);
            self.write_node(pool, &mut right)?;

            return Ok(());
        }

        // Parent is full — split the internal node
        self.split_internal(pool, parent, key, right_child)?;

        Ok(())
    }

    /// Split an internal node.
    fn split_internal(
        &mut self,
        pool: &mut BufferPoolManager,
        old_node: BTreeNode,
        new_key: &[u8],
        new_child: PageId,
    ) -> Result<()> {
        let (leftmost, mut entries) = old_node.internal_entries();

        // Find insertion point
        let pos = entries
            .binary_search_by(|(k, _)| k.as_slice().cmp(new_key))
            .unwrap_or_else(|i| i);
        entries.insert(pos, (new_key.to_vec(), new_child));

        let total = entries.len();
        let split_point = total / 2;

        // Left: entries[..split_point], with leftmost_child = leftmost
        let left_entries = &entries[..split_point];
        // The middle key gets pushed up to parent
        let push_up_key = entries[split_point].0.clone();
        // Right: entries[split_point+1..], with leftmost_child = right_child of split entry
        let right_leftmost = entries[split_point].1;
        let right_entries = &entries[split_point + 1..];

        // Create right internal node
        let right_id = self.allocate_page(pool)?;
        let mut right_node = BTreeNode::new_internal(right_id);
        right_node.set_level(old_node.get_level());
        right_node.internal_rebuild(right_leftmost, right_entries);

        // Rebuild left (old) node
        let old_id = old_node.page.id;
        let parent_id = old_node.get_parent();
        let level = old_node.get_level();

        let mut new_old_node = BTreeNode::new_internal(old_id);
        new_old_node.set_level(level);
        new_old_node.set_parent(parent_id);
        new_old_node.internal_rebuild(leftmost, left_entries);

        self.write_node(pool, &mut new_old_node)?;
        self.write_node(pool, &mut right_node)?;

        // Update children's parent pointers for right node
        self.update_children_parent(pool, right_id)?;

        // Push the middle key up to parent
        self.insert_into_parent(pool, old_id, &push_up_key, right_id, parent_id)?;

        Ok(())
    }

    /// Update all children of an internal node to point to it as parent.
    fn update_children_parent(
        &self,
        pool: &mut BufferPoolManager,
        parent_id: PageId,
    ) -> Result<()> {
        let parent = self.read_node(pool, parent_id)?;
        let n = parent.num_children();
        for i in 0..n {
            let child_id = parent.internal_child(i);
            let mut child = self.read_node(pool, child_id)?;
            child.set_parent(parent_id);
            self.write_node(pool, &mut child)?;
        }
        Ok(())
    }

    /// Delete a key from the tree. Returns the old value if found.
    ///
    /// Note: This implementation does not merge/redistribute underflowing nodes
    /// for simplicity. Deleted entries are simply removed from their leaf.
    /// In production you'd want rebalancing, but this is correct for correctness.
    pub fn delete(&mut self, pool: &mut BufferPoolManager, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let leaf_id = self.find_leaf(pool, key)?;
        let mut node = self.read_node(pool, leaf_id)?;

        let result = node.leaf_delete(key);
        if result.is_some() {
            self.write_node(pool, &mut node)?;
        }
        Ok(result)
    }

    /// Scan all key-value pairs in key order (using leaf sibling chain).
    pub fn scan_all(&self, pool: &mut BufferPoolManager) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        // Find the leftmost leaf
        let mut current_id = self.root_page_id;
        loop {
            let node = self.read_node(pool, current_id)?;
            if node.is_leaf() {
                break;
            }
            current_id = node.get_leftmost_child();
        }

        let mut result = Vec::new();
        loop {
            let node = self.read_node(pool, current_id)?;
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

    /// Range scan: return all KV pairs where start <= key <= end.
    pub fn range_scan(
        &self,
        pool: &mut BufferPoolManager,
        start: &[u8],
        end: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let leaf_id = self.find_leaf(pool, start)?;
        let mut current_id = leaf_id;
        let mut result = Vec::new();

        'outer: loop {
            let node = self.read_node(pool, current_id)?;
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
    use crate::disk::disk_manager::DiskManager;
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

    fn create_tree() -> (BPlusTree, BufferPoolManager, TempDir) {
        let dir = TempDir::new();
        let path = dir.path().join("test.db");
        let dm = DiskManager::open(&path).unwrap();
        let mut pool = BufferPoolManager::new(dm, 256);
        let tree = BPlusTree::create(&mut pool).unwrap();
        (tree, pool, dir)
    }

    #[test]
    fn test_insert_and_search() {
        let (mut tree, mut pool, _dir) = create_tree();

        assert!(tree.insert(&mut pool, b"hello", b"world").unwrap());
        assert!(tree.insert(&mut pool, b"foo", b"bar").unwrap());
        assert!(tree.insert(&mut pool, b"rust", b"lang").unwrap());

        assert_eq!(
            tree.search(&mut pool, b"hello").unwrap(),
            Some(b"world".to_vec())
        );
        assert_eq!(
            tree.search(&mut pool, b"foo").unwrap(),
            Some(b"bar".to_vec())
        );
        assert_eq!(
            tree.search(&mut pool, b"rust").unwrap(),
            Some(b"lang".to_vec())
        );
        assert_eq!(tree.search(&mut pool, b"missing").unwrap(), None);
    }

    #[test]
    fn test_update() {
        let (mut tree, mut pool, _dir) = create_tree();

        assert!(tree.insert(&mut pool, b"key", b"val1").unwrap());
        assert!(!tree.insert(&mut pool, b"key", b"val2").unwrap()); // update returns false
        assert_eq!(
            tree.search(&mut pool, b"key").unwrap(),
            Some(b"val2".to_vec())
        );
    }

    #[test]
    fn test_delete() {
        let (mut tree, mut pool, _dir) = create_tree();

        tree.insert(&mut pool, b"a", b"1").unwrap();
        tree.insert(&mut pool, b"b", b"2").unwrap();
        tree.insert(&mut pool, b"c", b"3").unwrap();

        assert_eq!(tree.delete(&mut pool, b"b").unwrap(), Some(b"2".to_vec()));
        assert_eq!(tree.search(&mut pool, b"b").unwrap(), None);
        assert_eq!(tree.search(&mut pool, b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(tree.search(&mut pool, b"c").unwrap(), Some(b"3".to_vec()));

        assert_eq!(tree.delete(&mut pool, b"missing").unwrap(), None);
    }

    #[test]
    fn test_leaf_split() {
        let (mut tree, mut pool, _dir) = create_tree();

        // Insert enough entries to trigger leaf splits
        for i in 0..500 {
            let key = format!("key{:05}", i);
            let val = format!("val{:05}", i);
            tree.insert(&mut pool, key.as_bytes(), val.as_bytes())
                .unwrap();
        }

        // Verify all entries
        for i in 0..500 {
            let key = format!("key{:05}", i);
            let val = format!("val{:05}", i);
            let result = tree.search(&mut pool, key.as_bytes()).unwrap();
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
        let (mut tree, mut pool, _dir) = create_tree();

        tree.insert(&mut pool, b"c", b"3").unwrap();
        tree.insert(&mut pool, b"a", b"1").unwrap();
        tree.insert(&mut pool, b"b", b"2").unwrap();

        let all = tree.scan_all(&mut pool).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], (b"a".to_vec(), b"1".to_vec()));
        assert_eq!(all[1], (b"b".to_vec(), b"2".to_vec()));
        assert_eq!(all[2], (b"c".to_vec(), b"3".to_vec()));
    }

    #[test]
    fn test_range_scan() {
        let (mut tree, mut pool, _dir) = create_tree();

        for i in 0..100 {
            let key = format!("k{:03}", i);
            let val = format!("v{:03}", i);
            tree.insert(&mut pool, key.as_bytes(), val.as_bytes())
                .unwrap();
        }

        let range = tree.range_scan(&mut pool, b"k020", b"k030").unwrap();
        assert_eq!(range.len(), 11); // k020..=k030
        assert_eq!(range[0].0, b"k020".to_vec());
        assert_eq!(range[10].0, b"k030".to_vec());
    }

    #[test]
    fn test_large_insert_and_delete() {
        let (mut tree, mut pool, _dir) = create_tree();

        let count = 1000;
        for i in 0..count {
            let key = format!("k{:06}", i);
            let val = format!("v{:06}", i);
            tree.insert(&mut pool, key.as_bytes(), val.as_bytes())
                .unwrap();
        }

        // Delete half
        for i in (0..count).step_by(2) {
            let key = format!("k{:06}", i);
            tree.delete(&mut pool, key.as_bytes()).unwrap();
        }

        // Verify remaining
        for i in 0..count {
            let key = format!("k{:06}", i);
            let result = tree.search(&mut pool, key.as_bytes()).unwrap();
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
