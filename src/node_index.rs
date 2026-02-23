use std::fs::File;
use std::io;
use std::path::Path;

use memmap2::MmapMut;

const ENTRY_SIZE: u64 = 8; // 4 bytes lat_e7 + 4 bytes lon_e7
const GROW_INCREMENT: u64 = 1_073_741_824; // 1 GB

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
        self.mmap[off..off + 4].copy_from_slice(&lat_e7.to_le_bytes());
        self.mmap[off + 4..off + 8].copy_from_slice(&lon_e7.to_le_bytes());
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
        let lat_e7 = i32::from_le_bytes(self.mmap[off..off + 4].try_into().unwrap());
        let lon_e7 = i32::from_le_bytes(self.mmap[off + 4..off + 8].try_into().unwrap());

        if lat_e7 == 0 && lon_e7 == 0 {
            None
        } else {
            Some((lat_e7, lon_e7))
        }
    }
}
