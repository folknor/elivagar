// Flat mmap'd node coordinate index.
//
// Direct-addressed at `node_id * 8`: for each node, stores 4 bytes lat_e7 +
// 4 bytes lon_e7. The file grows in 1 GB increments and is backed by mmap for
// zero-copy access.
//
// Planet scale: OSM has ~8.5B nodes with IDs up to ~12B, so the index file
// grows to ~96 GB.
//
// A two-level index (blocks of 4096 nodes) was considered but rejected:
// 12B / 4096 = ~2.9M blocks × 32 KB = ~93 GB — nearly identical to the flat
// index because OSM node IDs are distributed fairly continuously, not sparsely.
//
// madvise history: all madvise hints (MADV_SEQUENTIAL, MADV_RANDOM,
// MADV_HUGEPAGE, MADV_POPULATE_READ) were tried and removed. Every hint
// caused regressions because the index file is sparse — node IDs go up to
// ~12B regardless of dataset size, so a Denmark extract produces a ~96 GB
// file with only ~400 MB populated. Kernel default (MADV_NORMAL) works best.

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
}
