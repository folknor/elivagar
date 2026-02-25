use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use memmap2::{Mmap, MmapMut};

const ENTRY_SIZE: u64 = 12; // 8 bytes data_offset + 4 bytes coord_count
const GROW_INCREMENT: u64 = 1_073_741_824; // 1 GB
const COORD_SIZE: u64 = 8; // 4 bytes lat_e7 + 4 bytes lon_e7

// Safety: coords are stored as sequential LE i32 pairs matching (i32, i32) layout.
const _: () = assert!(std::mem::size_of::<(i32, i32)>() == 8);
const _: () = assert!(std::mem::align_of::<(i32, i32)>() == 4);
const _: () = assert!(cfg!(target_endian = "little"), "way_index assumes little-endian");

pub struct WayIndex {
    // Offset index (way_offsets.bin): mmap'd, indexed at way_id * 12
    offsets_file: File,
    offsets_mmap: MmapMut,
    offsets_file_len: u64,

    // Data file (way_data.bin): buffered writer during write phase, mmap after finish
    data_writer: Option<BufWriter<File>>,
    data_write_pos: u64,
    data_path: PathBuf,

    // Read-only mmap over way_data.bin, set after finish_writing()
    data_mmap: Option<Mmap>,
}

impl WayIndex {
    /// Create a new writable way index. Creates two files in `dir`:
    /// - `way_offsets.bin` -- indexed at way_id * 12, stores (u64 data_offset, u32 coord_count)
    /// - `way_data.bin` -- append-only packed coordinates
    pub fn create(dir: &Path) -> io::Result<Self> {
        let offsets_path = dir.join("way_offsets.bin");
        let data_path = dir.join("way_data.bin");

        let offsets_file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&offsets_path)?;

        let offsets_file_len = GROW_INCREMENT;
        offsets_file.set_len(offsets_file_len)?;

        let offsets_mmap = unsafe { MmapMut::map_mut(&offsets_file)? };


        let data_file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&data_path)?;

        let data_writer = Some(BufWriter::new(data_file));

        Ok(WayIndex {
            offsets_file,
            offsets_mmap,
            offsets_file_len,
            data_writer,
            data_write_pos: 0,
            data_path,
            data_mmap: None,
        })
    }

    /// Write geometry for a way. Appends coords to data file, records offset in index.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn put(&mut self, way_id: i64, coords: &[(i32, i32)]) {
        let coord_count = coords.len() as u32;

        // Record the current data write position as the offset for this way.
        let data_offset = self.data_write_pos;

        // Write coordinates to the data file.
        let writer = self.data_writer.as_mut().expect("put called after finish_writing");
        // Panic: unrecoverable I/O — disk full means the run is dead.
        for &(lat_e7, lon_e7) in coords {
            writer.write_all(&lat_e7.to_le_bytes()).expect("failed to write lat to way data");
            writer.write_all(&lon_e7.to_le_bytes()).expect("failed to write lon to way data");
        }
        self.data_write_pos += coord_count as u64 * COORD_SIZE;

        // Write the offset entry in the index file.
        let index_offset = way_id as u64 * ENTRY_SIZE;
        let needed = index_offset + ENTRY_SIZE;

        if needed > self.offsets_file_len {
            let mut new_len = self.offsets_file_len;
            while new_len < needed {
                new_len += GROW_INCREMENT;
            }
            // Panic: unrecoverable I/O — disk full or mmap failure means the run is dead.
            self.offsets_file.set_len(new_len).expect("failed to grow way offsets file");
            self.offsets_mmap = unsafe {
                MmapMut::map_mut(&self.offsets_file).expect("failed to remap way offsets")
            };

            self.offsets_file_len = new_len;
        }

        let off = index_offset as usize;
        self.offsets_mmap[off..off + 8].copy_from_slice(&data_offset.to_le_bytes());
        self.offsets_mmap[off + 8..off + 12].copy_from_slice(&coord_count.to_le_bytes());
    }

    /// Call after all ways have been written. Flushes the data writer and
    /// opens a read-only mmap over way_data.bin for random access reads.
    pub fn finish_writing(&mut self) -> io::Result<()> {
        // Flush and drop the BufWriter.
        if let Some(mut writer) = self.data_writer.take() {
            writer.flush()?;
        }

        // Open a read-only mmap over the data file (only if non-empty).
        if self.data_write_pos > 0 {
            let data_file = File::open(&self.data_path)?;
            let mmap = unsafe { Mmap::map(&data_file)? };
            self.data_mmap = Some(mmap);
        }

        Ok(())
    }

    /// Read geometry for a way as a zero-copy slice into the mmap.
    /// Only valid after `finish_writing()`.
    /// Returns `None` if entry is unset (offset and count both zero).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation, clippy::unwrap_used)]
    pub fn get(&self, way_id: i64) -> Option<&[(i32, i32)]> {
        let index_offset = way_id as u64 * ENTRY_SIZE;
        let needed = index_offset + ENTRY_SIZE;

        if needed > self.offsets_file_len {
            return None;
        }

        let off = index_offset as usize;
        // Infallible: slices are exactly 8 and 4 bytes by construction.
        let data_offset =
            u64::from_le_bytes(self.offsets_mmap[off..off + 8].try_into().unwrap());
        let coord_count =
            u32::from_le_bytes(self.offsets_mmap[off + 8..off + 12].try_into().unwrap());

        // Unset detection: both zero means no entry.
        if data_offset == 0 && coord_count == 0 {
            return None;
        }

        let mmap = self.data_mmap.as_ref()?;

        let start = data_offset as usize;
        let byte_len = coord_count as usize * COORD_SIZE as usize;
        let bytes = &mmap[start..start + byte_len];

        // Safety: data was written as sequential LE i32 pairs via put().
        // (i32, i32) is 8 bytes / 4-byte aligned (const-asserted above).
        // Mmap is page-aligned, data_offset is always a multiple of 8.
        let ptr = bytes.as_ptr().cast::<(i32, i32)>();
        Some(unsafe { std::slice::from_raw_parts(ptr, coord_count as usize) })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn create_and_get_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();
        idx.finish_writing().unwrap();
        assert!(idx.get(1).is_none());
    }

    #[test]
    fn put_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();
        let coords = [(10, 20), (30, 40), (50, 60)];
        idx.put(100, &coords);
        idx.finish_writing().unwrap();
        let result = idx.get(100).unwrap();
        assert_eq!(result, &coords);
    }

    #[test]
    fn put_multiple_ways() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();

        let coords_a = [(1, 2)];
        let coords_b = [(10, 20), (30, 40)];
        let coords_c = [(100, 200), (300, 400), (500, 600), (700, 800)];

        idx.put(5, &coords_a);
        idx.put(42, &coords_b);
        idx.put(999, &coords_c);
        idx.finish_writing().unwrap();

        assert_eq!(idx.get(5).unwrap(), &coords_a);
        assert_eq!(idx.get(42).unwrap(), &coords_b);
        assert_eq!(idx.get(999).unwrap(), &coords_c);
    }

    #[test]
    fn get_before_finish() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();
        idx.put(7, &[(1, 2), (3, 4)]);
        // data_mmap is None because finish_writing was never called,
        // so get returns None even though the offset entry exists.
        assert!(idx.get(7).is_none());
    }

    #[test]
    fn get_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();
        idx.put(1, &[(1, 2)]);
        idx.finish_writing().unwrap();
        // way_id 9999 was never written; its offset slot is zeroed out.
        assert!(idx.get(9999).is_none());
    }

    #[test]
    fn empty_way() {
        // Known edge case: putting an empty coords slice writes offset=current_pos
        // and count=0 into the offset entry. However, because data_write_pos starts
        // at 0 for the first insertion (and no bytes are appended), the entry is
        // (offset=0, count=0) which matches the sentinel for "unset". So get
        // returns None for an empty way that was the first insertion.
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();
        idx.put(50, &[]);
        idx.finish_writing().unwrap();
        // Sentinel (0, 0) is indistinguishable from unset — returns None.
        assert!(idx.get(50).is_none());
    }

    #[test]
    fn overwrite_way() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();

        let first = [(1, 1), (2, 2)];
        let second = [(10, 10), (20, 20), (30, 30)];

        idx.put(77, &first);
        idx.put(77, &second);
        idx.finish_writing().unwrap();

        // The second put overwrites the offset entry, so get returns the second value.
        let result = idx.get(77).unwrap();
        assert_eq!(result, &second);
    }
}
