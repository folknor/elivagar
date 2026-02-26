// Node coordinate index — two implementations.
//
// 1. NodeIndex (flat mmap): Direct-addressed at `node_id * 8`, 4 bytes lat_e7 +
//    4 bytes lon_e7. Grows in 1 GB increments, backed by mmap. Works for any
//    insertion order. Planet scale: ~96 GB file (IDs up to ~12B).
//
// 2. SortedNodeStore (compact in-RAM): Requires sorted insertion (ascending
//    node IDs, guaranteed by PBF Sort.Type_then_ID). Three-level hierarchy:
//    groups (64K nodes) → chunks (256 nodes) → individual nodes. Each level
//    uses a 256-bit bitmask for O(1) presence test + popcount-based indexing
//    into packed arrays. Denmark: ~420 MB vs 96 GB sparse mmap.
//
// NodeStore / NodeStoreReader enums dispatch to the appropriate backend.
//
// Flat mmap history: all madvise hints were tried and removed. See CLAUDE.md.

use std::fs::File;
use std::io;
use std::path::Path;

use memmap2::{Mmap, MmapMut};

const ENTRY_SIZE: u64 = 8; // 4 bytes lat_e7 + 4 bytes lon_e7
const GROW_INCREMENT: u64 = 1_073_741_824; // 1 GB

// XOR mask applied to stored coordinates so that (0,0) on disk means "unset"
// while a real node at lat=0, lon=0 stores as non-zero.
// 0x55555555 = 1431655765 E7 = 143.17° — outside valid latitude range [-90°, 90°],
// so no real coordinate pair can XOR to (0, 0).
const COORD_XOR: i32 = 0x5555_5555_u32 as i32;

pub struct NodeIndex {
    file: File,
    mmap: MmapMut,
    file_len: u64,
}

impl NodeIndex {
    /// Create a new writable node index file at `path`.
    ///
    /// No madvise hints are set during the write phase — see module-level
    /// comment for history on why MADV_SEQUENTIAL was removed.
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        let file_len = GROW_INCREMENT;
        file.set_len(file_len)?;

        let mmap = unsafe { MmapMut::map_mut(&file)? };

        Ok(NodeIndex {
            file,
            mmap,
            file_len,
        })
    }

    /// Write coordinates for a node. Grows the file if needed.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn put(&mut self, node_id: i64, lat_e7: i32, lon_e7: i32) {
        let offset = node_id as u64 * ENTRY_SIZE;
        let needed = offset + ENTRY_SIZE;

        if needed > self.file_len {
            // Grow in 1 GB increments until large enough.
            let mut new_len = self.file_len;
            while new_len < needed {
                new_len += GROW_INCREMENT;
            }
            // Panic: unrecoverable I/O — disk full or mmap failure means the run is dead.
            self.file.set_len(new_len).expect("failed to grow node index file");
            self.mmap = unsafe { MmapMut::map_mut(&self.file).expect("failed to remap node index") };

            self.file_len = new_len;
        }

        let off = offset as usize;
        self.mmap[off..off + 4].copy_from_slice(&(lat_e7 ^ COORD_XOR).to_le_bytes());
        self.mmap[off + 4..off + 8].copy_from_slice(&(lon_e7 ^ COORD_XOR).to_le_bytes());
    }

    /// Read coordinates for a node. Returns None if entry is unset (all zeros).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation, clippy::unwrap_used)]
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        get_from_mmap(&self.mmap, self.file_len, node_id)
    }

    /// Convert to a read-only reader after all nodes have been written.
    /// Consumes the writable index — no more `put()` calls are possible.
    ///
    /// The returned `NodeIndexReader` wraps a read-only `Mmap` which is `Sync`,
    /// allowing safe concurrent reads from rayon worker threads.
    pub fn into_reader(self) -> io::Result<NodeIndexReader> {
        let file_len = self.file_len;
        let mmap = self.mmap.make_read_only()?;
        Ok(NodeIndexReader { mmap, file_len })
    }
}

/// Shared get logic for both NodeIndex and NodeIndexReader.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation, clippy::unwrap_used)]
fn get_from_mmap(mmap: &[u8], file_len: u64, node_id: i64) -> Option<(i32, i32)> {
    let offset = node_id as u64 * ENTRY_SIZE;
    let needed = offset + ENTRY_SIZE;

    if needed > file_len {
        return None;
    }

    let off = offset as usize;
    // Infallible: slices are exactly 4 bytes by construction.
    let lat_raw = i32::from_le_bytes(mmap[off..off + 4].try_into().unwrap());
    let lon_raw = i32::from_le_bytes(mmap[off + 4..off + 8].try_into().unwrap());

    if lat_raw == 0 && lon_raw == 0 {
        None
    } else {
        Some((lat_raw ^ COORD_XOR, lon_raw ^ COORD_XOR))
    }
}

/// Read-only node coordinate index. Safe for concurrent reads from rayon threads
/// (`Mmap` is `Sync + Send`).
///
/// Created from `NodeIndex::into_reader()` after all nodes have been written.
pub struct NodeIndexReader {
    mmap: Mmap,
    file_len: u64,
}

impl NodeIndexReader {
    /// Read coordinates for a node. Returns None if entry is unset (all zeros).
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        get_from_mmap(&self.mmap, self.file_len, node_id)
    }
}

// ---------------------------------------------------------------------------
// SortedNodeStore — compact hierarchical store for sorted PBF files
// ---------------------------------------------------------------------------

const NODES_PER_CHUNK: usize = 256;
const NODES_PER_GROUP: u64 = 256 * 256; // 65536
const BITMASK_BYTES: usize = 32; // 256 bits

/// Set bit `pos` (0..255) in a 256-bit mask.
#[inline]
fn set_bit(mask: &mut [u8; BITMASK_BYTES], pos: u8) {
    mask[pos as usize / 8] |= 1 << (pos % 8);
}

/// Test whether bit `pos` (0..255) is set in a 256-bit mask.
#[inline]
fn test_bit(mask: &[u8; BITMASK_BYTES], pos: u8) -> bool {
    mask[pos as usize / 8] & (1 << (pos % 8)) != 0
}

/// Count set bits before position `pos` in a 256-bit mask.
/// This gives the index into the packed array for the element at `pos`.
#[inline]
fn count_bits_before(mask: &[u8; BITMASK_BYTES], pos: u8) -> usize {
    let byte_idx = pos as usize / 8;
    let bit_idx = pos % 8;
    let full: usize = mask[..byte_idx].iter().map(|b| b.count_ones() as usize).sum();
    let partial = (mask[byte_idx] & ((1u8 << bit_idx) - 1)).count_ones() as usize;
    full + partial
}

struct Chunk {
    node_mask: [u8; BITMASK_BYTES],
    coords: Box<[(i32, i32)]>,
}

struct Group {
    chunk_mask: [u8; BITMASK_BYTES],
    chunks: Box<[Chunk]>,
}

/// Look up a node within a finalized Group.
#[inline]
fn get_from_group(group: &Group, chunk_id: u8, node_in_chunk: u8) -> Option<(i32, i32)> {
    if !test_bit(&group.chunk_mask, chunk_id) {
        return None;
    }
    let chunk_idx = count_bits_before(&group.chunk_mask, chunk_id);
    let chunk = &group.chunks[chunk_idx];
    if !test_bit(&chunk.node_mask, node_in_chunk) {
        return None;
    }
    let node_idx = count_bits_before(&chunk.node_mask, node_in_chunk);
    Some(chunk.coords[node_idx])
}

/// Compact node coordinate store for sorted PBF files.
///
/// Requires node IDs to be inserted in strictly ascending order (guaranteed
/// by PBF `Sort.Type_then_ID`). Builds a 3-level hierarchy: groups (64K
/// nodes each) → chunks (256 nodes each) → individual nodes. Each level
/// uses a 256-bit bitmask for O(1) presence test and popcount-based indexing.
///
/// Memory: ~8 bytes per node + negligible overhead.
/// Denmark (52.5M nodes): ~420 MB vs 96 GB for the flat mmap index.
pub struct SortedNodeStore {
    /// Completed groups, indexed by group_id. None = no nodes in that group.
    groups: Vec<Option<Box<Group>>>,

    // Builder state for the group being accumulated.
    current_group_id: u64,
    current_chunk_mask: [u8; BITMASK_BYTES],
    current_chunks: Vec<Chunk>,

    // Builder state for the chunk being accumulated.
    current_chunk_id: u8,
    current_node_mask: [u8; BITMASK_BYTES],
    current_coords: Vec<(i32, i32)>,

    last_node_id: i64,
    node_count: u64,
}

impl SortedNodeStore {
    pub fn new() -> Self {
        SortedNodeStore {
            groups: Vec::new(),
            current_group_id: 0,
            current_chunk_mask: [0u8; BITMASK_BYTES],
            current_chunks: Vec::new(),
            current_chunk_id: 0,
            current_node_mask: [0u8; BITMASK_BYTES],
            current_coords: Vec::with_capacity(NODES_PER_CHUNK),
            last_node_id: -1,
            node_count: 0,
        }
    }

    /// Store coordinates for a node. Node IDs MUST be strictly increasing.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn put(&mut self, node_id: i64, lat_e7: i32, lon_e7: i32) {
        debug_assert!(
            node_id > self.last_node_id,
            "SortedNodeStore: node IDs must be strictly increasing, got {node_id} after {}",
            self.last_node_id
        );
        self.last_node_id = node_id;
        self.node_count += 1;

        let id = node_id as u64;
        let group_id = id / NODES_PER_GROUP;
        let chunk_id = ((id % NODES_PER_GROUP) / NODES_PER_CHUNK as u64) as u8;
        let node_in_chunk = (id % NODES_PER_CHUNK as u64) as u8;

        if self.node_count == 1 {
            // First node: initialize builder state.
            if self.groups.len() <= group_id as usize {
                self.groups.resize_with(group_id as usize + 1, || None);
            }
            self.current_group_id = group_id;
            self.current_chunk_id = chunk_id;
        } else if group_id != self.current_group_id {
            // New group: flush current chunk + group.
            self.flush_chunk();
            self.flush_group();
            if self.groups.len() <= group_id as usize {
                self.groups.resize_with(group_id as usize + 1, || None);
            }
            self.current_group_id = group_id;
            self.current_chunk_mask = [0u8; BITMASK_BYTES];
            self.current_chunks = Vec::new();
            self.current_chunk_id = chunk_id;
            self.current_node_mask = [0u8; BITMASK_BYTES];
            self.current_coords = Vec::with_capacity(NODES_PER_CHUNK);
        } else if chunk_id != self.current_chunk_id {
            // Same group, new chunk: flush current chunk.
            self.flush_chunk();
            self.current_chunk_id = chunk_id;
            self.current_node_mask = [0u8; BITMASK_BYTES];
            self.current_coords = Vec::with_capacity(NODES_PER_CHUNK);
        }

        set_bit(&mut self.current_node_mask, node_in_chunk);
        self.current_coords.push((lat_e7, lon_e7));
    }

    fn flush_chunk(&mut self) {
        if self.current_coords.is_empty() {
            return;
        }
        set_bit(&mut self.current_chunk_mask, self.current_chunk_id);
        let coords: Box<[(i32, i32)]> = self.current_coords.drain(..).collect();
        self.current_chunks.push(Chunk {
            node_mask: self.current_node_mask,
            coords,
        });
    }

    fn flush_group(&mut self) {
        if self.current_chunks.is_empty() {
            return;
        }
        let group = Group {
            chunk_mask: self.current_chunk_mask,
            chunks: std::mem::take(&mut self.current_chunks).into_boxed_slice(),
        };
        let gid = self.current_group_id as usize;
        self.groups[gid] = Some(Box::new(group));
    }

    /// Convert to a read-only reader. Consumes the store.
    pub fn into_reader(mut self) -> SortedNodeStoreReader {
        self.flush_chunk();
        self.flush_group();
        SortedNodeStoreReader {
            groups: self.groups,
        }
    }

    /// Read coordinates (used by tests; not performance-critical).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        let id = node_id as u64;
        let group_id = id / NODES_PER_GROUP;
        let chunk_id = ((id % NODES_PER_GROUP) / NODES_PER_CHUNK as u64) as u8;
        let node_in_chunk = (id % NODES_PER_CHUNK as u64) as u8;

        // Check in-progress group.
        if self.node_count > 0 && group_id == self.current_group_id {
            // Check in-progress chunk first.
            if chunk_id == self.current_chunk_id {
                if !test_bit(&self.current_node_mask, node_in_chunk) {
                    return None;
                }
                let idx = count_bits_before(&self.current_node_mask, node_in_chunk);
                return Some(self.current_coords[idx]);
            }
            // Check completed chunks in current group.
            if test_bit(&self.current_chunk_mask, chunk_id) {
                let chunk_idx = count_bits_before(&self.current_chunk_mask, chunk_id);
                let chunk = &self.current_chunks[chunk_idx];
                if !test_bit(&chunk.node_mask, node_in_chunk) {
                    return None;
                }
                let node_idx = count_bits_before(&chunk.node_mask, node_in_chunk);
                return Some(chunk.coords[node_idx]);
            }
            return None;
        }

        // Check finalized groups.
        let group = self.groups.get(group_id as usize)?.as_ref()?;
        get_from_group(group, chunk_id, node_in_chunk)
    }

    /// Number of nodes stored.
    pub fn node_count(&self) -> u64 {
        self.node_count
    }
}

/// Read-only sorted node coordinate store. Safe for concurrent reads
/// from rayon threads (all interior data is immutable).
pub struct SortedNodeStoreReader {
    groups: Vec<Option<Box<Group>>>,
}

impl SortedNodeStoreReader {
    /// Look up coordinates for a node. O(1) via bitmask popcount.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        let id = node_id as u64;
        let group_id = (id / NODES_PER_GROUP) as usize;
        let chunk_id = ((id % NODES_PER_GROUP) / NODES_PER_CHUNK as u64) as u8;
        let node_in_chunk = (id % NODES_PER_CHUNK as u64) as u8;

        let group = self.groups.get(group_id)?.as_ref()?;
        get_from_group(group, chunk_id, node_in_chunk)
    }
}

// ---------------------------------------------------------------------------
// NodeStore / NodeStoreReader — enum dispatch
// ---------------------------------------------------------------------------

/// Unified node store for the write phase.
pub enum NodeStore {
    Flat(NodeIndex),
    Sorted(SortedNodeStore),
}

impl NodeStore {
    pub fn put(&mut self, node_id: i64, lat_e7: i32, lon_e7: i32) {
        match self {
            NodeStore::Flat(idx) => idx.put(node_id, lat_e7, lon_e7),
            NodeStore::Sorted(store) => store.put(node_id, lat_e7, lon_e7),
        }
    }

    pub fn into_reader(self) -> io::Result<NodeStoreReader> {
        match self {
            NodeStore::Flat(idx) => Ok(NodeStoreReader::Flat(idx.into_reader()?)),
            NodeStore::Sorted(store) => Ok(NodeStoreReader::Sorted(store.into_reader())),
        }
    }
}

/// Unified read-only node store for the parallel processing phase.
pub enum NodeStoreReader {
    Flat(NodeIndexReader),
    Sorted(SortedNodeStoreReader),
}

impl NodeStoreReader {
    pub fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        match self {
            NodeStoreReader::Flat(reader) => reader.get(node_id),
            NodeStoreReader::Sorted(reader) => reader.get(node_id),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn create_and_get_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let idx = NodeIndex::create(&path).unwrap();
        assert_eq!(idx.get(1), None);
    }

    #[test]
    fn put_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(100, 555_000_000, 133_000_000);
        assert_eq!(idx.get(100), Some((555_000_000, 133_000_000)));
    }

    #[test]
    fn put_multiple_get_each() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(10, 100_000, 200_000);
        idx.put(20, 300_000, 400_000);
        idx.put(30, 500_000, 600_000);
        assert_eq!(idx.get(10), Some((100_000, 200_000)));
        assert_eq!(idx.get(20), Some((300_000, 400_000)));
        assert_eq!(idx.get(30), Some((500_000, 600_000)));
    }

    #[test]
    fn get_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(5, 111, 222);
        assert_eq!(idx.get(999), None);
    }

    #[test]
    fn zero_zero_is_valid() {
        // Nodes at lat=0, lon=0 (Gulf of Guinea) should be stored and retrieved correctly.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(1, 0, 0);
        assert_eq!(idx.get(1), Some((0, 0)));
    }

    #[test]
    fn overwrite_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(42, 111_000, 222_000);
        idx.put(42, 333_000, 444_000);
        assert_eq!(idx.get(42), Some((333_000, 444_000)));
    }

    #[test]
    fn into_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut idx = NodeIndex::create(&path).unwrap();
        idx.put(10, 100_000, 200_000);
        idx.put(20, 300_000, 400_000);

        let reader = idx.into_reader().unwrap();
        assert_eq!(reader.get(10), Some((100_000, 200_000)));
        assert_eq!(reader.get(20), Some((300_000, 400_000)));
        assert_eq!(reader.get(999), None);
    }

    // --- Bitmask helper tests ---

    #[test]
    fn bitmask_set_and_test() {
        let mut mask = [0u8; BITMASK_BYTES];
        assert!(!test_bit(&mask, 0));
        set_bit(&mut mask, 0);
        assert!(test_bit(&mask, 0));
        assert!(!test_bit(&mask, 1));

        set_bit(&mut mask, 255);
        assert!(test_bit(&mask, 255));
        assert!(!test_bit(&mask, 254));
    }

    #[test]
    fn bitmask_count_before() {
        let mut mask = [0u8; BITMASK_BYTES];
        set_bit(&mut mask, 3);
        set_bit(&mut mask, 7);
        set_bit(&mut mask, 10);
        assert_eq!(count_bits_before(&mask, 3), 0);
        assert_eq!(count_bits_before(&mask, 7), 1);
        assert_eq!(count_bits_before(&mask, 10), 2);
        assert_eq!(count_bits_before(&mask, 200), 3);
    }

    #[test]
    fn bitmask_count_before_dense() {
        let mask = [0xFF_u8; BITMASK_BYTES];
        assert_eq!(count_bits_before(&mask, 0), 0);
        assert_eq!(count_bits_before(&mask, 128), 128);
        assert_eq!(count_bits_before(&mask, 255), 255);
    }

    // --- SortedNodeStore tests ---

    #[test]
    fn sorted_create_and_get_empty() {
        let store = SortedNodeStore::new();
        assert_eq!(store.get(1), None);
    }

    #[test]
    fn sorted_put_and_get() {
        let mut store = SortedNodeStore::new();
        store.put(100, 555_000_000, 133_000_000);
        assert_eq!(store.get(100), Some((555_000_000, 133_000_000)));
    }

    #[test]
    fn sorted_put_multiple_get_each() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100_000, 200_000);
        store.put(20, 300_000, 400_000);
        store.put(30, 500_000, 600_000);
        assert_eq!(store.get(10), Some((100_000, 200_000)));
        assert_eq!(store.get(20), Some((300_000, 400_000)));
        assert_eq!(store.get(30), Some((500_000, 600_000)));
    }

    #[test]
    fn sorted_get_nonexistent() {
        let mut store = SortedNodeStore::new();
        store.put(5, 111, 222);
        assert_eq!(store.get(999), None);
    }

    #[test]
    fn sorted_zero_zero_is_valid() {
        let mut store = SortedNodeStore::new();
        store.put(1, 0, 0);
        assert_eq!(store.get(1), Some((0, 0)));
    }

    #[test]
    fn sorted_into_reader() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100_000, 200_000);
        store.put(20, 300_000, 400_000);
        let reader = store.into_reader();
        assert_eq!(reader.get(10), Some((100_000, 200_000)));
        assert_eq!(reader.get(20), Some((300_000, 400_000)));
        assert_eq!(reader.get(999), None);
    }

    #[test]
    fn sorted_cross_chunk_boundary() {
        let mut store = SortedNodeStore::new();
        store.put(255, 10, 20);
        store.put(256, 30, 40);
        assert_eq!(store.get(255), Some((10, 20)));
        assert_eq!(store.get(256), Some((30, 40)));
    }

    #[test]
    fn sorted_cross_group_boundary() {
        let mut store = SortedNodeStore::new();
        store.put(65535, 10, 20);
        store.put(65536, 30, 40);
        let reader = store.into_reader();
        assert_eq!(reader.get(65535), Some((10, 20)));
        assert_eq!(reader.get(65536), Some((30, 40)));
    }

    #[test]
    fn sorted_large_gap_in_ids() {
        let mut store = SortedNodeStore::new();
        store.put(1_000_000, 10, 20);
        store.put(10_000_000, 30, 40);
        let reader = store.into_reader();
        assert_eq!(reader.get(1_000_000), Some((10, 20)));
        assert_eq!(reader.get(10_000_000), Some((30, 40)));
        assert_eq!(reader.get(5_000_000), None);
    }

    #[test]
    fn sorted_single_node() {
        let mut store = SortedNodeStore::new();
        store.put(42, 100, 200);
        let reader = store.into_reader();
        assert_eq!(reader.get(42), Some((100, 200)));
        assert_eq!(reader.get(41), None);
        assert_eq!(reader.get(43), None);
    }

    #[test]
    fn sorted_dense_chunk() {
        let mut store = SortedNodeStore::new();
        for i in 0..256i64 {
            #[allow(clippy::cast_possible_truncation)]
            store.put(i, i as i32 * 100, i as i32 * 200);
        }
        let reader = store.into_reader();
        for i in 0..256i64 {
            #[allow(clippy::cast_possible_truncation)]
            {
                assert_eq!(reader.get(i), Some((i as i32 * 100, i as i32 * 200)));
            }
        }
    }

    #[test]
    fn sorted_empty_into_reader() {
        let store = SortedNodeStore::new();
        let reader = store.into_reader();
        assert_eq!(reader.get(0), None);
        assert_eq!(reader.get(1_000_000), None);
    }

    #[test]
    fn sorted_high_node_id() {
        let mut store = SortedNodeStore::new();
        store.put(12_000_000_000, 550_000_000, 130_000_000);
        let reader = store.into_reader();
        assert_eq!(reader.get(12_000_000_000), Some((550_000_000, 130_000_000)));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "strictly increasing")]
    fn sorted_rejects_non_monotonic() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100, 200);
        store.put(5, 300, 400);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "strictly increasing")]
    fn sorted_rejects_duplicate_id() {
        let mut store = SortedNodeStore::new();
        store.put(10, 100, 200);
        store.put(10, 300, 400);
    }

    #[test]
    fn node_store_sorted_variant() {
        let mut store = NodeStore::Sorted(SortedNodeStore::new());
        store.put(100, 555_000_000, 133_000_000);
        let reader = store.into_reader().unwrap();
        assert_eq!(reader.get(100), Some((555_000_000, 133_000_000)));
        assert_eq!(reader.get(999), None);
    }

    #[test]
    fn node_store_flat_variant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_index_test.bin");
        let mut store = NodeStore::Flat(NodeIndex::create(&path).unwrap());
        store.put(100, 555_000_000, 133_000_000);
        let reader = store.into_reader().unwrap();
        assert_eq!(reader.get(100), Some((555_000_000, 133_000_000)));
        assert_eq!(reader.get(999), None);
    }
}
