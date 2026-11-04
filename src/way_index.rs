use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::node_index::LatLonE7;
use memmap2::Mmap;

const OFFSET_ENTRY_SIZE: usize = 16; // 8 bytes way_id + 8 bytes data_offset

struct WayEntry {
    way_id: WayId,
    data_offset: u64,
}
const _: () = assert!(std::mem::size_of::<WayEntry>() == OFFSET_ENTRY_SIZE);

/// An OSM way identifier, kept distinct from node and relation identifiers.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WayId(i64);

impl WayId {
    pub const fn new(raw: i64) -> Self {
        Self(raw)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

/// 256 MB sort budget = 16M entries per chunk.
/// North America (210M ways) ≈ 13 chunks. Denmark fits in 1 chunk.
const OFFSETS_SORT_BUDGET: usize = 256 * 1024 * 1024;
const ENTRIES_PER_CHUNK: usize = OFFSETS_SORT_BUDGET / OFFSET_ENTRY_SIZE;

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

fn read_varint(data: &[u8], pos: &mut usize) -> io::Result<u32> {
    let mut result: u32 = 0;
    for shift in (0..=28).step_by(7) {
        let b = *data
            .get(*pos)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated way varint"))?;
        *pos += 1;
        if shift == 28 && b & 0xF0 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "way varint exceeds u32",
            ));
        }
        result |= u32::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "way varint exceeds five bytes",
    ))
}

/// Encode a way's coordinates: varint coord_count + first coord raw + deltas as zigzag varints.
#[allow(clippy::cast_possible_truncation)]
fn encode_way(buf: &mut Vec<u8>, coords: &[LatLonE7]) {
    write_varint(buf, coords.len() as u32);
    let LatLonE7 {
        lat: mut prev_lat,
        lon: mut prev_lon,
    } = coords[0];
    buf.extend_from_slice(&prev_lat.to_le_bytes());
    buf.extend_from_slice(&prev_lon.to_le_bytes());
    for coord in &coords[1..] {
        write_varint(buf, zigzag_encode(coord.lat.wrapping_sub(prev_lat)));
        write_varint(buf, zigzag_encode(coord.lon.wrapping_sub(prev_lon)));
        prev_lat = coord.lat;
        prev_lon = coord.lon;
    }
}

/// Decode a way's coordinates from the compressed data buffer at the given offset.
fn decode_way(data: &[u8], offset: usize) -> io::Result<Vec<LatLonE7>> {
    let mut pos = offset;
    let count = usize::try_from(read_varint(data, &mut pos)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "way coordinate count overflow"))?;
    if count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "way coordinate count is zero",
        ));
    }
    let min_payload = (count - 1)
        .checked_mul(2)
        .and_then(|deltas| deltas.checked_add(8))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "way coordinate count overflow")
        })?;
    if data.len().saturating_sub(pos) < min_payload {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated way coordinates",
        ));
    }
    let mut coords = Vec::with_capacity(count.min(1 << 20));

    let lat = i32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
    let lon = i32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
    pos += 8;
    coords.push(LatLonE7::new(lat, lon));

    let mut prev_lat = lat;
    let mut prev_lon = lon;
    for _ in 1..count {
        let dlat = zigzag_decode(read_varint(data, &mut pos)?);
        let dlon = zigzag_decode(read_varint(data, &mut pos)?);
        prev_lat = prev_lat.wrapping_add(dlat);
        prev_lon = prev_lon.wrapping_add(dlon);
        coords.push(LatLonE7::new(prev_lat, prev_lon));
    }
    Ok(coords)
}

// --- Offset entry I/O helpers ---

/// Read way_id from a sorted offsets mmap at entry index `idx`.
fn mmap_way_id(mmap: &[u8], idx: usize) -> WayId {
    let off = idx * OFFSET_ENTRY_SIZE;
    WayId::new(i64::from_le_bytes(
        mmap[off..off + 8]
            .try_into()
            .expect("offset mmap too short"),
    ))
}

/// Read data_offset from a sorted offsets mmap at entry index `idx`.
#[allow(clippy::cast_possible_truncation)]
fn mmap_data_offset(mmap: &[u8], idx: usize) -> usize {
    let off = idx * OFFSET_ENTRY_SIZE + 8;
    u64::from_le_bytes(
        mmap[off..off + 8]
            .try_into()
            .expect("offset mmap too short"),
    ) as usize
}

/// Binary search for `way_id` in a sorted offsets mmap with `count` entries.
fn mmap_binary_search(mmap: &[u8], count: usize, way_id: WayId) -> Option<usize> {
    let mut lo: usize = 0;
    let mut hi: usize = count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match mmap_way_id(mmap, mid).cmp(&way_id) {
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
            Ordering::Equal => return Some(mid),
        }
    }
    None
}

/// Read `count` WayEntry records from a BufReader.
fn read_entries(reader: &mut BufReader<File>, count: usize) -> io::Result<Vec<WayEntry>> {
    let mut entries = Vec::with_capacity(count);
    let mut buf = [0u8; OFFSET_ENTRY_SIZE];
    for _ in 0..count {
        reader.read_exact(&mut buf)?;
        let way_id = WayId::new(i64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]));
        let data_offset = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        entries.push(WayEntry {
            way_id,
            data_offset,
        });
    }
    Ok(entries)
}

/// Write WayEntry records as raw 16-byte LE pairs.
fn write_entries(writer: &mut BufWriter<File>, entries: &[WayEntry]) -> io::Result<()> {
    for e in entries {
        writer.write_all(&e.way_id.as_i64().to_le_bytes())?;
        writer.write_all(&e.data_offset.to_le_bytes())?;
    }
    writer.flush()?;
    Ok(())
}

// --- External sort for offset entries ---

/// Sequential reader for a sorted chunk of 16-byte offset entries.
struct OffsetChunkReader {
    reader: BufReader<File>,
    remaining: usize,
}

impl OffsetChunkReader {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        #[allow(clippy::cast_possible_truncation)]
        let len = file.metadata()?.len() as usize;
        let remaining = len / OFFSET_ENTRY_SIZE;
        Ok(OffsetChunkReader {
            reader: BufReader::with_capacity(1 << 20, file),
            remaining,
        })
    }

    fn read_entry(&mut self) -> io::Result<Option<(WayId, u64)>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let mut buf = [0u8; OFFSET_ENTRY_SIZE];
        self.reader.read_exact(&mut buf)?;
        self.remaining -= 1;
        let way_id = WayId::new(i64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]));
        let data_offset = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        Ok(Some((way_id, data_offset)))
    }
}

/// Merge heap entry - smallest way_id wins (reverse Ord for max-heap).
struct OffsetHeapEntry {
    way_id: WayId,
    data_offset: u64,
    chunk_idx: usize,
}

impl Eq for OffsetHeapEntry {}

impl PartialEq for OffsetHeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.way_id == other.way_id && self.chunk_idx == other.chunk_idx
    }
}

impl Ord for OffsetHeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse: smallest way_id first from max-heap
        other
            .way_id
            .cmp(&self.way_id)
            .then_with(|| other.chunk_idx.cmp(&self.chunk_idx))
    }
}

impl PartialOrd for OffsetHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// K-way merge of sorted chunk files into a single sorted output.
fn merge_offset_chunks(chunk_paths: &[PathBuf], sorted_path: &Path) -> io::Result<()> {
    let mut readers: Vec<OffsetChunkReader> = Vec::with_capacity(chunk_paths.len());
    let mut heap = BinaryHeap::with_capacity(chunk_paths.len());

    for (idx, path) in chunk_paths.iter().enumerate() {
        let mut cr = OffsetChunkReader::open(path)?;
        if let Some((way_id, data_offset)) = cr.read_entry()? {
            heap.push(OffsetHeapEntry {
                way_id,
                data_offset,
                chunk_idx: idx,
            });
        }
        readers.push(cr);
    }

    let out_file = File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(sorted_path)?;
    let mut writer = BufWriter::with_capacity(1 << 20, out_file);

    while let Some(entry) = heap.pop() {
        writer.write_all(&entry.way_id.as_i64().to_le_bytes())?;
        writer.write_all(&entry.data_offset.to_le_bytes())?;

        let idx = entry.chunk_idx;
        if let Some((way_id, data_offset)) = readers[idx].read_entry()? {
            heap.push(OffsetHeapEntry {
                way_id,
                data_offset,
                chunk_idx: idx,
            });
        }
    }

    writer.flush()?;
    Ok(())
}

/// External sort of the unsorted offsets file by way_id.
/// Uses a 256 MB memory budget: reads chunks, sorts in RAM, writes temp files,
/// then k-way merges. Fast path for single-chunk datasets (no temp files).
fn sort_offsets_file(unsorted_path: &Path, sorted_path: &Path) -> io::Result<usize> {
    let file = File::open(unsorted_path)?;
    #[allow(clippy::cast_possible_truncation)]
    let file_len = file.metadata()?.len() as usize;
    let entry_count = file_len / OFFSET_ENTRY_SIZE;

    if entry_count == 0 {
        File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(sorted_path)?;
        return Ok(0);
    }

    let mut reader = BufReader::with_capacity(1 << 20, file);

    if entry_count <= ENTRIES_PER_CHUNK {
        // Fast path: everything fits in one chunk - sort in memory, write directly
        let mut entries = read_entries(&mut reader, entry_count)?;
        entries.sort_unstable_by_key(|e| e.way_id);
        let out_file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(sorted_path)?;
        let mut writer = BufWriter::with_capacity(1 << 20, out_file);
        write_entries(&mut writer, &entries)?;
        return Ok(entry_count);
    }

    // Multi-chunk external sort
    let dir = unsorted_path
        .parent()
        .expect("offsets file has no parent dir");
    let mut chunk_paths: Vec<PathBuf> = Vec::new();
    let mut remaining = entry_count;

    while remaining > 0 {
        let chunk_size = remaining.min(ENTRIES_PER_CHUNK);
        let mut entries = read_entries(&mut reader, chunk_size)?;
        entries.sort_unstable_by_key(|e| e.way_id);

        let chunk_path = dir.join(format!("way_offsets_chunk_{}.bin", chunk_paths.len()));
        let chunk_file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&chunk_path)?;
        let mut writer = BufWriter::with_capacity(1 << 20, chunk_file);
        write_entries(&mut writer, &entries)?;
        chunk_paths.push(chunk_path);
        remaining -= chunk_size;
    }
    // Drop reader to close unsorted file before merge
    drop(reader);

    merge_offset_chunks(&chunk_paths, sorted_path)?;

    // Cleanup temp chunk files
    for p in &chunk_paths {
        drop(std::fs::remove_file(p));
    }

    Ok(entry_count)
}

// --- WayIndex ---

pub struct WayIndexWriter {
    offsets_writer: BufWriter<File>,
    data_writer: BufWriter<File>,
    data_write_pos: u64,
    encode_buf: Vec<u8>,
    way_count: u64,
    offsets_path: PathBuf,
    data_path: PathBuf,
    sorted_offsets_path: PathBuf,
}

pub struct WayIndexReader {
    offsets_mmap: Option<Mmap>,
    data_mmap: Option<Mmap>,
    entry_count: usize,
    way_count: u64,
}

impl WayIndexWriter {
    /// Create a new writable way index. Creates two sequential temp files in `dir`:
    /// - `way_offsets.bin` - (way_id, data_offset) entries, 16 bytes each
    /// - `way_data.bin` - delta-varint compressed coordinates
    pub fn create(dir: &Path) -> io::Result<Self> {
        let offsets_path = dir.join("way_offsets.bin");
        let data_path = dir.join("way_data.bin");
        let sorted_offsets_path = dir.join("way_offsets_sorted.bin");

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

        Ok(WayIndexWriter {
            offsets_writer: BufWriter::with_capacity(65536, offsets_file),
            data_writer: BufWriter::with_capacity(65536, data_file),
            data_write_pos: 0,
            encode_buf: Vec::with_capacity(256),
            way_count: 0,
            offsets_path,
            data_path,
            sorted_offsets_path,
        })
    }

    /// Write geometry for a way. Delta-varint encodes coords and appends to data file,
    /// records (way_id, offset) in the offsets file.
    pub fn put(&mut self, way_id: WayId, coords: &[LatLonE7]) {
        if coords.is_empty() {
            return;
        }

        let data_offset = self.data_write_pos;

        // Encode compressed way into scratch buffer
        self.encode_buf.clear();
        encode_way(&mut self.encode_buf, coords);

        // Write compressed data
        // Panic: unrecoverable I/O - disk full means the run is dead.
        self.data_writer
            .write_all(&self.encode_buf)
            .expect("failed to write way data");
        self.data_write_pos += self.encode_buf.len() as u64;

        // Write offset entry: way_id (i64 LE) + data_offset (u64 LE) = 16 bytes
        self.offsets_writer
            .write_all(&way_id.as_i64().to_le_bytes())
            .expect("failed to write way offset");
        self.offsets_writer
            .write_all(&data_offset.to_le_bytes())
            .expect("failed to write way offset");

        self.way_count += 1;
    }

    /// Call after all ways have been written. Sorts offset entries by way_id on disk
    /// via external merge sort, then mmaps both sorted offsets and compressed data
    /// for zero-heap-allocation reads during relation processing.
    pub fn finish_writing(mut self) -> io::Result<WayIndexReader> {
        self.offsets_writer.flush()?;
        self.data_writer.flush()?;
        drop(self.offsets_writer);
        drop(self.data_writer);
        // Release encode scratch buffer
        self.encode_buf = Vec::new();

        if self.way_count == 0 {
            return Ok(WayIndexReader {
                offsets_mmap: None,
                data_mmap: None,
                entry_count: 0,
                way_count: 0,
            });
        }

        // External sort offsets by way_id → sorted file
        let entry_count = sort_offsets_file(&self.offsets_path, &self.sorted_offsets_path)?;

        // Remove unsorted offsets - no longer needed
        drop(std::fs::remove_file(&self.offsets_path));

        // Mmap sorted offsets (read-only) for binary search
        let offsets_file = File::open(&self.sorted_offsets_path)?;
        // SAFETY: file is written and flushed by sort_offsets_file above,
        // no other process modifies it.
        let offsets_mmap = Some(unsafe { Mmap::map(&offsets_file)? });

        // Mmap compressed data (read-only) for decode
        let data_file = File::open(&self.data_path)?;
        // SAFETY: file is written and flushed above, no other process modifies it.
        let data_mmap = Some(unsafe { Mmap::map(&data_file)? });

        let data_bytes = data_mmap.as_ref().map_or(0, |m| m.len());
        let index_bytes = entry_count * OFFSET_ENTRY_SIZE;
        crate::debug::emit_counter_u64("way_index_ways", self.way_count);
        crate::debug::emit_counter_usize("way_index_data_bytes", data_bytes);
        crate::debug::emit_counter_usize("way_index_index_bytes", index_bytes);
        let data_mb = data_bytes as f64 / (1024.0 * 1024.0);
        let index_mb = index_bytes as f64 / (1024.0 * 1024.0);
        eprintln!(
            "  Way index: {} ways, {data_mb:.1} MB compressed data, {index_mb:.1} MB index (mmap'd)",
            self.way_count
        );

        Ok(WayIndexReader {
            offsets_mmap,
            data_mmap,
            entry_count,
            way_count: self.way_count,
        })
    }
}

impl WayIndexReader {
    /// Look up geometry for a way by ID. Only valid after `finish_writing()`.
    /// Returns decoded coordinates, None if way_id was never stored, or an
    /// error if the indexed data is corrupt.
    pub fn get(&self, way_id: WayId) -> io::Result<Option<Vec<LatLonE7>>> {
        if self.way_count == 0 {
            return Ok(None);
        }
        let offsets = self.offsets_mmap.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "finished way index has no offsets map",
            )
        })?;
        let Some(idx) = mmap_binary_search(offsets, self.entry_count, way_id) else {
            return Ok(None);
        };
        let data_offset = mmap_data_offset(offsets, idx);
        let data = self.data_mmap.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "finished way index has no data map",
            )
        })?;
        decode_way(data, data_offset).map(Some)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn create_and_get_empty() {
        let dir = tempfile::tempdir().unwrap();
        let idx = WayIndexWriter::create(dir.path())
            .unwrap()
            .finish_writing()
            .unwrap();
        assert!(idx.get(WayId::new(1)).unwrap().is_none());
    }

    #[test]
    fn put_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndexWriter::create(dir.path()).unwrap();
        let coords = [
            LatLonE7::new(10, 20),
            LatLonE7::new(30, 40),
            LatLonE7::new(50, 60),
        ];
        idx.put(WayId::new(100), &coords);
        let idx = idx.finish_writing().unwrap();
        let result = idx.get(WayId::new(100)).unwrap().unwrap();
        assert_eq!(result, coords);
    }

    #[test]
    fn put_multiple_ways() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndexWriter::create(dir.path()).unwrap();

        let coords_a = [LatLonE7::new(1, 2)];
        let coords_b = [LatLonE7::new(10, 20), LatLonE7::new(30, 40)];
        let coords_c = [
            LatLonE7::new(100, 200),
            LatLonE7::new(300, 400),
            LatLonE7::new(500, 600),
            LatLonE7::new(700, 800),
        ];

        idx.put(WayId::new(5), &coords_a);
        idx.put(WayId::new(42), &coords_b);
        idx.put(WayId::new(999), &coords_c);
        let idx = idx.finish_writing().unwrap();

        assert_eq!(idx.get(WayId::new(5)).unwrap().unwrap(), coords_a);
        assert_eq!(idx.get(WayId::new(42)).unwrap().unwrap(), coords_b);
        assert_eq!(idx.get(WayId::new(999)).unwrap().unwrap(), coords_c);
    }

    #[test]
    fn finish_produces_readable_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndexWriter::create(dir.path()).unwrap();
        idx.put(WayId::new(7), &[LatLonE7::new(1, 2), LatLonE7::new(3, 4)]);
        let idx = idx.finish_writing().unwrap();
        assert_eq!(
            idx.get(WayId::new(7)).unwrap().unwrap(),
            [LatLonE7::new(1, 2), LatLonE7::new(3, 4)]
        );
    }

    #[test]
    fn get_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndexWriter::create(dir.path()).unwrap();
        idx.put(WayId::new(1), &[LatLonE7::new(1, 2)]);
        let idx = idx.finish_writing().unwrap();
        // way_id 9999 was never written.
        assert!(idx.get(WayId::new(9999)).unwrap().is_none());
    }

    #[test]
    fn empty_way() {
        // put() with empty coords is a no-op - no entry written.
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndexWriter::create(dir.path()).unwrap();
        let empty: [LatLonE7; 0] = [];
        idx.put(WayId::new(50), &empty);
        let idx = idx.finish_writing().unwrap();
        assert!(idx.get(WayId::new(50)).unwrap().is_none());
    }

    #[test]
    fn roundtrip_delta_heavy() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = WayIndexWriter::create(dir.path()).unwrap();
        let coords = [
            LatLonE7::new(550_000_000, 120_000_000),
            LatLonE7::new(550_000_001, 120_000_000), // tiny delta
            LatLonE7::new(550_000_001, 120_000_000), // zero delta
            LatLonE7::new(-200_000_000, -400_000_000), // large negative jump
            LatLonE7::new(0, 0),                     // jump to origin
        ];
        idx.put(WayId::new(1), &coords);
        let idx = idx.finish_writing().unwrap();
        assert_eq!(idx.get(WayId::new(1)).unwrap().unwrap(), coords);
    }

    #[test]
    fn way_delta_varint_roundtrips_full_i32_range() {
        let mut coords = vec![
            LatLonE7::new(0, -1_799_000_000),
            LatLonE7::new(0, 1_799_000_000),
            LatLonE7::new(i32::MAX, i32::MIN),
            LatLonE7::new(i32::MIN, i32::MAX),
            LatLonE7::new(i32::MIN, i32::MIN),
            LatLonE7::new(i32::MAX, i32::MAX),
        ];

        // Generate deterministic values across the full bit range without a test dependency.
        let mut state = 0x9E37_79B9_u32;
        for _ in 0..4096 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let lat = i32::from_le_bytes(state.to_le_bytes());

            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let lon = i32::from_le_bytes(state.to_le_bytes());
            coords.push(LatLonE7::new(lat, lon));
        }

        let mut encoded = Vec::new();
        encode_way(&mut encoded, &coords);
        assert_eq!(decode_way(&encoded, 0).unwrap(), coords);
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
        let mut idx = WayIndexWriter::create(dir.path()).unwrap();
        idx.put(WayId::new(500), &[LatLonE7::new(50, 60)]);
        idx.put(WayId::new(100), &[LatLonE7::new(10, 20)]);
        idx.put(WayId::new(300), &[LatLonE7::new(30, 40)]);
        let idx = idx.finish_writing().unwrap();

        assert_eq!(
            idx.get(WayId::new(100)).unwrap().unwrap(),
            [LatLonE7::new(10, 20)]
        );
        assert_eq!(
            idx.get(WayId::new(300)).unwrap().unwrap(),
            [LatLonE7::new(30, 40)]
        );
        assert_eq!(
            idx.get(WayId::new(500)).unwrap().unwrap(),
            [LatLonE7::new(50, 60)]
        );
        assert!(idx.get(WayId::new(200)).unwrap().is_none());
    }
}
