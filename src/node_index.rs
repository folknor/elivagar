use std::fs::File;
use std::io;
use std::path::Path;

use memmap2::MmapMut;

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
        let offset = node_id as u64 * ENTRY_SIZE;
        let needed = offset + ENTRY_SIZE;

        if needed > self.file_len {
            return None;
        }

        let off = offset as usize;
        let lat_raw = i32::from_le_bytes(self.mmap[off..off + 4].try_into().unwrap());
        let lon_raw = i32::from_le_bytes(self.mmap[off + 4..off + 8].try_into().unwrap());

        if lat_raw == 0 && lon_raw == 0 {
            None // unwritten entry (mmap zero-fills)
        } else {
            Some((lat_raw ^ COORD_XOR, lon_raw ^ COORD_XOR))
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
}
