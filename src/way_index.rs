use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

const OFFSET_ENTRY_SIZE: usize = 16; // 8 bytes way_id + 8 bytes data_offset

struct WayEntry {
    way_id: i64,
    data_offset: u64,
}
const _: () = assert!(std::mem::size_of::<WayEntry>() == OFFSET_ENTRY_SIZE);

// --- Varint helpers ---

#[allow(clippy::cast_possible_wrap)]
fn zigzag_encode(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)).cast_unsigned()
}

#[allow(clippy::cast_possible_wrap)]
fn zigzag_decode(v: u32) -> i32 {
    (v >> 1) as i32 ^ -((v & 1) as i32)
}

#[allow(clippy::cast_possible_truncation)]
fn write_varint(buf: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn read_varint(data: &[u8], pos: &mut usize) -> u32 {
    let mut result: u32 = 0;
    let mut shift = 0;
    loop {
        let b = data[*pos];
        *pos += 1;
        result |= u32::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return result;
        }
        shift += 7;
    }
}

/// Encode a way's coordinates: varint coord_count + first coord raw + deltas as zigzag varints.
#[allow(clippy::cast_possible_truncation)]
fn encode_way(buf: &mut Vec<u8>, coords: &[(i32, i32)]) {
    write_varint(buf, coords.len() as u32);
    let (mut prev_lat, mut prev_lon) = coords[0];
    buf.extend_from_slice(&prev_lat.to_le_bytes());
    buf.extend_from_slice(&prev_lon.to_le_bytes());
    for &(lat, lon) in &coords[1..] {
        write_varint(buf, zigzag_encode(lat - prev_lat));
        write_varint(buf, zigzag_encode(lon - prev_lon));
        prev_lat = lat;
        prev_lon = lon;
    }
}

/// Decode a way's coordinates from the compressed data buffer at the given offset.
fn decode_way(data: &[u8], offset: usize) -> Vec<(i32, i32)> {
    let mut pos = offset;
    let count = read_varint(data, &mut pos) as usize;
    let mut coords = Vec::with_capacity(count);

    let lat = i32::from_le_bytes(
        data[pos..pos + 4].try_into().expect("truncated way data"),
    );
    let lon = i32::from_le_bytes(
        data[pos + 4..pos + 8].try_into().expect("truncated way data"),
    );
    pos += 8;
    coords.push((lat, lon));

    let mut prev_lat = lat;
    let mut prev_lon = lon;
    for _ in 1..count {
        let dlat = zigzag_decode(read_varint(data, &mut pos));
        let dlon = zigzag_decode(read_varint(data, &mut pos));
        prev_lat += dlat;
        prev_lon += dlon;
        coords.push((prev_lat, prev_lon));
    }
    coords
}

// --- WayIndex ---

pub struct WayIndex {
    // Write phase: sequential BufWriters to temp files
    offsets_writer: Option<BufWriter<File>>,
    data_writer: Option<BufWriter<File>>,
    data_write_pos: u64,
    encode_buf: Vec<u8>,
    way_count: u64,
    offsets_path: PathBuf,
    data_path: PathBuf,

    // Read phase: populated by finish_writing()
    entries: Vec<WayEntry>,
    data: Vec<u8>,
}

impl WayIndex {
    /// Create a new writable way index. Creates two sequential temp files in `dir`:
    /// - `way_offsets.bin` — (way_id, data_offset) entries, 16 bytes each
    /// - `way_data.bin` — delta-varint compressed coordinates
    pub fn create(dir: &Path) -> io::Result<Self> {
        let offsets_path = dir.join("way_offsets.bin");
        let data_path = dir.join("way_data.bin");

        let offsets_file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&offsets_path)?;

        let data_file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&data_path)?;

        Ok(WayIndex {
            offsets_writer: Some(BufWriter::with_capacity(65536, offsets_file)),
            data_writer: Some(BufWriter::with_capacity(65536, data_file)),
            data_write_pos: 0,
            encode_buf: Vec::with_capacity(256),
            way_count: 0,
            offsets_path,
            data_path,
            entries: Vec::new(),
            data: Vec::new(),
        })
    }

    /// Write geometry for a way. Delta-varint encodes coords and appends to data file,
    /// records (way_id, offset) in the offsets file.
    pub fn put(&mut self, way_id: i64, coords: &[(i32, i32)]) {
        if coords.is_empty() {
            return;
        }

        let data_offset = self.data_write_pos;

        // Encode compressed way into scratch buffer
        self.encode_buf.clear();
        encode_way(&mut self.encode_buf, coords);

        // Write compressed data
        let writer = self.data_writer.as_mut().expect("put called after finish_writing");
        // Panic: unrecoverable I/O — disk full means the run is dead.
        writer
            .write_all(&self.encode_buf)
            .expect("failed to write way data");
        self.data_write_pos += self.encode_buf.len() as u64;

        // Write offset entry: way_id (i64 LE) + data_offset (u64 LE) = 16 bytes
        let owriter = self
            .offsets_writer
            .as_mut()
            .expect("put called after finish_writing");
        owriter
            .write_all(&way_id.to_le_bytes())
            .expect("failed to write way offset");
        owriter
            .write_all(&data_offset.to_le_bytes())
            .expect("failed to write way offset");

        self.way_count += 1;
    }

    /// Call after all ways have been written. Loads compressed data and offset index
    /// into memory, sorts entries by way_id for binary search during relation processing.
    pub fn finish_writing(&mut self) -> io::Result<()> {
        // Flush and drop writers
        if let Some(mut w) = self.offsets_writer.take() {
            w.flush()?;
        }
        if let Some(mut w) = self.data_writer.take() {
            w.flush()?;
        }
        // Release encode scratch buffer
        self.encode_buf = Vec::new();

        if self.way_count == 0 {
            return Ok(());
        }

        // Read compressed data into memory
        self.data = std::fs::read(&self.data_path)?;

        // Read offset entries into Vec<WayEntry>, sort by way_id
        let offsets_bytes = std::fs::read(&self.offsets_path)?;
        let entry_count = offsets_bytes.len() / OFFSET_ENTRY_SIZE;
        let mut entries = Vec::with_capacity(entry_count);
        for i in 0..entry_count {
            let off = i * OFFSET_ENTRY_SIZE;
            // Infallible: slices are exactly 8 bytes by construction (file is N * 16 bytes).
            let way_id = i64::from_le_bytes([
                offsets_bytes[off],
                offsets_bytes[off + 1],
                offsets_bytes[off + 2],
                offsets_bytes[off + 3],
                offsets_bytes[off + 4],
                offsets_bytes[off + 5],
                offsets_bytes[off + 6],
                offsets_bytes[off + 7],
            ]);
            let data_offset = u64::from_le_bytes([
                offsets_bytes[off + 8],
                offsets_bytes[off + 9],
                offsets_bytes[off + 10],
                offsets_bytes[off + 11],
                offsets_bytes[off + 12],
                offsets_bytes[off + 13],
                offsets_bytes[off + 14],
                offsets_bytes[off + 15],
            ]);
            entries.push(WayEntry {
                way_id,
                data_offset,
            });
        }
        drop(offsets_bytes);

        entries.sort_unstable_by_key(|e| e.way_id);
        self.entries = entries;

        let data_mb = self.data.len() as f64 / (1024.0 * 1024.0);
        let index_mb = (self.entries.len() * OFFSET_ENTRY_SIZE) as f64 / (1024.0 * 1024.0);
        eprintln!(
            "  Way index: {} ways, {data_mb:.1} MB compressed data, {index_mb:.1} MB index",
            self.way_count
        );

        Ok(())
    }

    /// Look up geometry for a way by ID. Only valid after `finish_writing()`.
    /// Returns decoded coordinates, or None if way_id was never stored.
    #[allow(clippy::cast_possible_truncation)]
    pub fn get(&self, way_id: i64) -> Option<Vec<(i32, i32)>> {
        let idx = self
            .entries
            .binary_search_by_key(&way_id, |e| e.way_id)
            .ok()?;
        let offset = self.entries[idx].data_offset as usize;
        Some(decode_way(&self.data, offset))
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
        assert_eq!(result, coords);
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

        assert_eq!(idx.get(5).unwrap(), coords_a);
        assert_eq!(idx.get(42).unwrap(), coords_b);
        assert_eq!(idx.get(999).unwrap(), coords_c);
    }

    #[test]
    fn get_before_finish() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();
        idx.put(7, &[(1, 2), (3, 4)]);
        // entries is empty before finish_writing, so binary search returns None.
        assert!(idx.get(7).is_none());
    }

    #[test]
    fn get_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();
        idx.put(1, &[(1, 2)]);
        idx.finish_writing().unwrap();
        // way_id 9999 was never written.
        assert!(idx.get(9999).is_none());
    }

    #[test]
    fn empty_way() {
        // put() with empty coords is a no-op — no entry written.
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();
        idx.put(50, &[]);
        idx.finish_writing().unwrap();
        assert!(idx.get(50).is_none());
    }

    #[test]
    fn roundtrip_delta_heavy() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();
        let coords = [
            (550_000_000, 120_000_000),
            (550_000_001, 120_000_000),   // tiny delta
            (550_000_001, 120_000_000),   // zero delta
            (-200_000_000, -400_000_000), // large negative jump
            (0, 0),                       // jump to origin
        ];
        idx.put(1, &coords);
        idx.finish_writing().unwrap();
        assert_eq!(idx.get(1).unwrap(), coords);
    }

    #[test]
    fn zigzag_roundtrip() {
        for v in [0, 1, -1, 127, -128, i32::MAX, i32::MIN] {
            assert_eq!(zigzag_decode(zigzag_encode(v)), v);
        }
    }

    #[test]
    fn unsorted_way_ids() {
        // Ways may arrive out of order from parallel processing.
        // finish_writing sorts them for binary search.
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndex::create(dir.path()).unwrap();
        idx.put(500, &[(50, 60)]);
        idx.put(100, &[(10, 20)]);
        idx.put(300, &[(30, 40)]);
        idx.finish_writing().unwrap();

        assert_eq!(idx.get(100).unwrap(), [(10, 20)]);
        assert_eq!(idx.get(300).unwrap(), [(30, 40)]);
        assert_eq!(idx.get(500).unwrap(), [(50, 60)]);
        assert!(idx.get(200).is_none());
    }
}
