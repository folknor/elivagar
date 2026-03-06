// External merge sort for tile feature records.
//
// Designed for planet-scale data (~100+ GB of sort records). Records are
// buffered in memory up to a configurable chunk size, flushed as sorted chunk
// files, then merged via a k-way merge using a binary heap.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

// ---------------------------------------------------------------------------
// Sort key helpers
// ---------------------------------------------------------------------------

/// Sort key: u64 encoding `(tile_id << 16) | (layer << 8) | priority`.
pub type SortKey = u64;

/// Build a sort key from tile id, layer index, and priority.
///
/// Packs tile_id into bits 63-16 (48-bit field). Overflows above z23, but
/// max_zoom is validated to 14 at pipeline entry (max tile_id ~358M = 29 bits).
#[inline]
pub fn make_sort_key(tile_id: u64, layer: u8, priority: u8) -> SortKey {
    (tile_id << 16) | (u64::from(layer) << 8) | u64::from(priority)
}

/// Extract tile_id from a sort key.
#[inline]
pub fn tile_id_from_key(key: SortKey) -> u64 {
    key >> 16
}

/// Extract the layer index from a sort key.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn layer_from_key(key: SortKey) -> u8 {
    ((key >> 8) & 0xFF) as u8
}

// ---------------------------------------------------------------------------
// SortRecord
// ---------------------------------------------------------------------------

/// A record in the sort buffer: a sort key plus opaque payload bytes.
///
/// `data` must be an owned Vec because records are serialized to chunk files on
/// disk and deserialized during k-way merge — there is no lifetime to reference
/// into. Arena allocation was considered and rejected: it would require
/// redesigning the chunk file format (currently per-record `key|len|data`), the
/// ChunkReader, and the HeapEntry ownership model, for minimal runtime benefit
/// since mimalloc handles the small allocs efficiently.
pub struct SortRecord {
    pub key: SortKey,
    pub data: Box<[u8]>,
}
const _: () = assert!(std::mem::size_of::<SortRecord>() == 24);

// ---------------------------------------------------------------------------
// SortWriter
// ---------------------------------------------------------------------------

/// Buffered writer that accepts sort records, flushes sorted chunks to disk
/// when the buffer exceeds the target size, and produces a `SortReader` for
/// the merge phase.
pub struct SortWriter {
    tmp_dir: PathBuf,
    buffer: Vec<SortRecord>,
    buffer_bytes: usize,
    chunk_size_bytes: usize,
    chunk_paths: Vec<PathBuf>,
    chunk_count: usize,
}

impl SortWriter {
    /// Create a new sort writer. `chunk_size_bytes` is the target memory
    /// budget per chunk (typically ~1 GB).
    pub fn new(tmp_dir: &Path, chunk_size_bytes: usize) -> io::Result<Self> {
        fs::create_dir_all(tmp_dir)?;
        Ok(SortWriter {
            tmp_dir: tmp_dir.to_path_buf(),
            buffer: Vec::new(),
            buffer_bytes: 0,
            chunk_size_bytes,
            chunk_paths: Vec::new(),
            chunk_count: 0,
        })
    }

    /// Resume a sort writer with existing chunk files in the tmp dir.
    /// `start_chunk` is the number of chunks to keep (from a previous phase);
    /// any chunks beyond that are deleted (leftovers from a previous run).
    pub fn resume(tmp_dir: &Path, chunk_size_bytes: usize, start_chunk: usize) -> io::Result<Self> {
        let mut chunk_paths: Vec<PathBuf> = Vec::with_capacity(start_chunk);
        for i in 0..start_chunk {
            let path = tmp_dir.join(format!("chunk_{i:04}.bin"));
            if !path.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing chunk file: {}", path.display()),
                ));
            }
            chunk_paths.push(path);
        }

        // Delete leftover chunks from a previous run. Scan beyond gaps to
        // catch stale chunks that would otherwise contaminate a later sort.
        {
            let mut i = start_chunk;
            let mut gap_count = 0;
            while gap_count < 10 {
                let path = tmp_dir.join(format!("chunk_{i:04}.bin"));
                if path.exists() {
                    fs::remove_file(&path)?;
                    gap_count = 0;
                } else {
                    gap_count += 1;
                }
                i += 1;
            }
        }

        Ok(SortWriter {
            tmp_dir: tmp_dir.to_path_buf(),
            buffer: Vec::new(),
            buffer_bytes: 0,
            chunk_size_bytes,
            chunk_paths,
            chunk_count: start_chunk,
        })
    }

    /// Number of chunk files written so far (including adopted ones from resume).
    pub fn chunk_count(&self) -> usize {
        self.chunk_count
    }

    /// Add a record to the buffer. If the buffer exceeds `chunk_size_bytes`,
    /// the current buffer is sorted and flushed to a chunk file on disk.
    pub fn push(&mut self, record: SortRecord) -> io::Result<()> {
        self.buffer_bytes += record.data.len() + std::mem::size_of::<SortRecord>();
        self.buffer.push(record);
        if self.buffer_bytes >= self.chunk_size_bytes {
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// Flush the in-memory buffer to a chunk file if non-empty.
    /// Call this before saving a checkpoint so `chunk_count()` is accurate.
    pub fn flush(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// Flush the remaining buffer and return a `SortReader` for the k-way
    /// merge phase.
    pub fn finish(mut self) -> io::Result<SortReader> {
        self.flush()?;
        SortReader::new(&self.chunk_paths)
    }

    /// Read accessor for the temporary directory.
    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    /// Read accessor for the chunk size budget.
    pub fn chunk_size_bytes(&self) -> usize {
        self.chunk_size_bytes
    }

    /// Adopt externally-written chunk files (e.g., from parallel ocean processing).
    /// Files must be in standard chunk format (sorted records). The chunk_count is
    /// updated so that subsequent flushes and `from_dir` scans remain consistent.
    pub fn adopt_chunk_files(&mut self, paths: Vec<PathBuf>) {
        self.chunk_count += paths.len();
        self.chunk_paths.extend(paths);
    }

    /// Sort the in-memory buffer by key and write a chunk file to disk.
    #[allow(clippy::cast_possible_truncation)]
    fn flush_chunk(&mut self) -> io::Result<()> {
        let path = self.tmp_dir.join(format!("chunk_{:04}.bin", self.chunk_count));
        write_sorted_chunk(&mut self.buffer, &path)?;

        self.chunk_paths.push(path);
        self.chunk_count += 1;
        self.buffer.clear();
        self.buffer_bytes = 0;
        Ok(())
    }
}

/// Write records as a sorted chunk file in the standard format.
///
/// Records are sorted in-place by key, then written as:
/// ```text
/// u32 record_count
/// For each record:
///   u64 key
///   u32 data_len
///   [u8; data_len] data
/// ```
///
/// Used by `SortWriter::flush_chunk` and by parallel ocean processing
/// (each rayon worker flushes its own chunk files directly).
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
pub fn write_sorted_chunk(records: &mut [SortRecord], path: &Path) -> io::Result<()> {
    records.sort_unstable_by_key(|r| r.key);

    let file = File::create(path)?;
    let buf = BufWriter::with_capacity(1 << 20, file);
    let mut writer = GzEncoder::new(buf, Compression::new(1));

    // Record count as u32. Safe: 1 GB chunk budget yields max ~48.8M records
    // (minimum 22 bytes each), 88x below u32::MAX.
    let count = records.len() as u32;
    writer.write_all(&count.to_le_bytes())?;

    for record in records.iter() {
        writer.write_all(&record.key.to_le_bytes())?;
        let data_len = record.data.len() as u32;
        writer.write_all(&data_len.to_le_bytes())?;
        writer.write_all(&record.data)?;
    }

    writer.finish()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// ChunkReader — reads records sequentially from a single chunk file
// ---------------------------------------------------------------------------

struct ChunkReader {
    reader: GzDecoder<BufReader<File>>,
    remaining: u32,
}

impl ChunkReader {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let buf = BufReader::with_capacity(256 * 1024, file);
        let mut reader = GzDecoder::new(buf);

        let mut buf4 = [0u8; 4];
        reader.read_exact(&mut buf4)?;
        let remaining = u32::from_le_bytes(buf4);

        Ok(ChunkReader { reader, remaining })
    }

    // Allocates a Vec per record. A reusable buffer was considered but the heap
    // holds only k entries (1-4 chunks for Denmark, ~20 for planet) and records
    // vary in size, so a pool would often reallocate anyway. Not a bottleneck.
    fn read_record(&mut self) -> io::Result<Option<(SortKey, Box<[u8]>)>> {
        if self.remaining == 0 {
            return Ok(None);
        }

        let mut buf8 = [0u8; 8];
        self.reader.read_exact(&mut buf8)?;
        let key = u64::from_le_bytes(buf8);

        let mut buf4 = [0u8; 4];
        self.reader.read_exact(&mut buf4)?;
        let data_len = u32::from_le_bytes(buf4);

        let mut data = vec![0u8; data_len as usize].into_boxed_slice();
        self.reader.read_exact(&mut data)?;

        self.remaining -= 1;
        Ok(Some((key, data)))
    }

}

// ---------------------------------------------------------------------------
// HeapEntry — element in the merge heap
// ---------------------------------------------------------------------------

struct HeapEntry {
    key: SortKey,
    data: Box<[u8]>,
    chunk_idx: usize,
}
const _: () = assert!(std::mem::size_of::<HeapEntry>() == 32);

impl Eq for HeapEntry {}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.chunk_idx == other.chunk_idx
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering: smallest key should come out first from a max-heap
        // wrapped in `Reverse`. We compare (key, chunk_idx).
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.chunk_idx.cmp(&self.chunk_idx))
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------
// SortReader — k-way merge of sorted chunk files
// ---------------------------------------------------------------------------

/// Reads sorted records from multiple chunk files using a k-way merge.
pub struct SortReader {
    chunk_readers: Vec<ChunkReader>,
    heap: BinaryHeap<HeapEntry>,
}

impl SortReader {
    /// Open all chunk files found in a directory and create a merge reader.
    ///
    /// `expected_chunks`: if `Some(n)`, verifies exactly `n` contiguous chunk files exist.
    /// Detects stale leftover chunks from a previous run that could silently contaminate
    /// the merge. Pass `None` to skip validation (not recommended for `--skip-to sort`).
    pub fn from_dir(tmp_dir: &Path, expected_chunks: Option<usize>) -> io::Result<Self> {
        let mut chunk_paths: Vec<PathBuf> = Vec::new();
        let mut i = 0;
        loop {
            let path = tmp_dir.join(format!("chunk_{i:04}.bin"));
            if path.exists() {
                chunk_paths.push(path);
                i += 1;
            } else {
                break;
            }
        }
        if let Some(expected) = expected_chunks
            && chunk_paths.len() != expected
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "chunk count mismatch: found {} but checkpoint expects {}. \
                     Stale chunks from a previous run may be present — \
                     run a full pipeline (without --skip-to) to regenerate.",
                    chunk_paths.len(),
                    expected,
                ),
            ));
        }
        Self::new(&chunk_paths)
    }

    /// Open all chunk files and prime the merge heap with the first record
    /// from each chunk.
    fn new(chunk_paths: &[PathBuf]) -> io::Result<Self> {
        let mut chunk_readers = Vec::with_capacity(chunk_paths.len());
        let mut heap = BinaryHeap::with_capacity(chunk_paths.len());

        for (idx, path) in chunk_paths.iter().enumerate() {
            let mut cr = ChunkReader::open(path)?;
            if let Some((key, data)) = cr.read_record()? {
                heap.push(HeapEntry {
                    key,
                    data,
                    chunk_idx: idx,
                });
            }
            chunk_readers.push(cr);
        }

        Ok(SortReader {
            chunk_readers,
            heap,
        })
    }

    /// Return the next record in globally sorted order, or `None` when all
    /// records have been consumed.
    ///
    /// Named `next` for clarity, but can't implement `Iterator` because iteration
    /// is fallible (`io::Result`). The `fallible-iterator` crate isn't worth the dep.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> io::Result<Option<SortRecord>> {
        let entry = match self.heap.pop() {
            Some(e) => e,
            None => return Ok(None),
        };

        let idx = entry.chunk_idx;
        let result = SortRecord {
            key: entry.key,
            data: entry.data,
        };

        // Read the next record from the same chunk and push it onto the heap.
        // When a chunk is fully consumed, advise the kernel to evict its pages
        // from the page cache — at planet scale this frees 100+ GB for the
        // assemble phase's PMTiles read-back.
        if let Some((key, data)) = self.chunk_readers[idx].read_record()? {
            self.heap.push(HeapEntry {
                key,
                data,
                chunk_idx: idx,
            });
        }

        Ok(Some(result))
    }

}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn collect_keys(mut reader: SortReader) -> Vec<u64> {
        let mut out = Vec::new();
        while let Some(rec) = reader.next().unwrap() {
            out.push(rec.key);
        }
        out
    }

    #[test]
    fn sort_key_round_trip() {
        let cases: Vec<(u64, u8, u8)> = vec![
            (0, 0, 0),
            (1, 2, 3),
            (0xFF_FFFF_FFFF_FFFF, 0xFF, 0xFF),
            (42, 7, 128),
            (1_000_000, 0, 255),
        ];
        for (tile_id, layer, priority) in cases {
            // tile_id is only 48 bits wide in the encoding
            let tile_id = tile_id & 0x0000_FFFF_FFFF_FFFF;
            let key = make_sort_key(tile_id, layer, priority);
            assert_eq!(
                tile_id_from_key(key),
                tile_id,
                "tile_id mismatch for ({tile_id}, {layer}, {priority})"
            );
            assert_eq!(
                layer_from_key(key),
                layer,
                "layer mismatch for ({tile_id}, {layer}, {priority})"
            );
        }
    }

    #[test]
    fn single_chunk_sort() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Use a large chunk size so everything fits in one chunk.
        let mut writer = SortWriter::new(dir.path(), 1_000_000).unwrap();

        // Push 1000 records with random-ish keys.
        let mut expected_keys: Vec<u64> = Vec::with_capacity(1000);
        for i in 0u64..1000 {
            // Simple scramble: reverse bits of i within a small range.
            let key = (i.wrapping_mul(7919)) ^ (i << 3);
            expected_keys.push(key);
            writer
                .push(SortRecord {
                    key,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
        }

        let mut reader = writer.finish().unwrap();

        // Collect all output records and verify global sort order.
        let mut output_keys: Vec<u64> = Vec::new();
        while let Some(record) = reader.next().unwrap() {
            output_keys.push(record.key);
        }

        assert_eq!(output_keys.len(), 1000);
        for i in 1..output_keys.len() {
            assert!(
                output_keys[i - 1] <= output_keys[i],
                "not sorted at index {i}: {} > {}",
                output_keys[i - 1],
                output_keys[i]
            );
        }

        // Verify all expected keys are present.
        expected_keys.sort();
        assert_eq!(output_keys, expected_keys);
    }

    #[test]
    fn multi_chunk_sort() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Very small chunk size forces multiple chunks.
        let mut writer = SortWriter::new(dir.path(), 100).unwrap();

        let mut expected_keys: Vec<u64> = Vec::with_capacity(500);
        for i in 0u64..500 {
            let key = (i.wrapping_mul(6271)) ^ (i << 5);
            expected_keys.push(key);
            writer
                .push(SortRecord {
                    key,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
        }

        // Should have produced multiple chunk files.
        assert!(
            writer.chunk_count > 1,
            "expected multiple chunks, got {}",
            writer.chunk_count
        );

        let mut reader = writer.finish().unwrap();

        let mut output_keys: Vec<u64> = Vec::new();
        while let Some(record) = reader.next().unwrap() {
            output_keys.push(record.key);
        }

        assert_eq!(output_keys.len(), 500);
        for i in 1..output_keys.len() {
            assert!(
                output_keys[i - 1] <= output_keys[i],
                "not sorted at index {i}: {} > {}",
                output_keys[i - 1],
                output_keys[i]
            );
        }

        expected_keys.sort();
        assert_eq!(output_keys, expected_keys);
    }

    #[test]
    fn empty_input() {
        let dir = tempfile::tempdir().expect("create tempdir");

        let writer = SortWriter::new(dir.path(), 1_000_000).unwrap();
        let mut reader = writer.finish().unwrap();

        assert!(reader.next().unwrap().is_none());
    }

    #[test]
    fn duplicate_keys() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Very small chunk size to force multi-chunk even with few records.
        let mut writer = SortWriter::new(dir.path(), 50).unwrap();

        // 100 records all with the same key but different data.
        for i in 0u32..100 {
            writer
                .push(SortRecord {
                    key: 42,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
        }

        let mut reader = writer.finish().unwrap();

        let mut count = 0u32;
        while let Some(record) = reader.next().unwrap() {
            assert_eq!(record.key, 42);
            count += 1;
            assert_eq!(record.data.len(), 4);
        }
        assert_eq!(count, 100);
    }

    #[test]
    fn data_integrity() {
        let dir = tempfile::tempdir().expect("create tempdir");

        let mut writer = SortWriter::new(dir.path(), 200).unwrap();

        // Push records with keys and payload that can be verified.
        for i in 0u64..50 {
            let key = 50 - i; // descending keys
            let mut data = Vec::new();
            data.extend_from_slice(&key.to_le_bytes());
            data.extend_from_slice(b"payload_");
            data.extend_from_slice(&i.to_le_bytes());
            writer.push(SortRecord { key, data: data.into_boxed_slice() }).unwrap();
        }

        let mut reader = writer.finish().unwrap();

        // Records should come out sorted by key (ascending: 1, 2, ..., 50).
        let mut prev_key = 0u64;
        let mut count = 0u64;
        while let Some(record) = reader.next().unwrap() {
            assert!(record.key >= prev_key);
            prev_key = record.key;
            count += 1;

            // Verify the data payload starts with the key.
            let embedded_key = u64::from_le_bytes(
                record.data[..8].try_into().unwrap(),
            );
            assert_eq!(embedded_key, record.key);
        }
        assert_eq!(count, 50);
    }

    #[test]
    fn from_dir_accepts_matching_chunk_count() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 100).unwrap();
        for i in 0u64..200 {
            writer.push(SortRecord { key: i, data: Box::from(i.to_le_bytes().as_slice()) }).unwrap();
        }
        let n = writer.chunk_count();
        assert!(n > 1, "need multiple chunks for meaningful test");
        // finish() consumes writer but chunks remain on disk
        let _ = writer.finish().unwrap();

        // Exact match passes
        let reader = SortReader::from_dir(dir.path(), Some(n));
        assert!(reader.is_ok());
    }

    #[test]
    fn from_dir_rejects_chunk_count_mismatch() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 100).unwrap();
        for i in 0u64..200 {
            writer.push(SortRecord { key: i, data: Box::from(i.to_le_bytes().as_slice()) }).unwrap();
        }
        let n = writer.chunk_count();
        let _ = writer.finish().unwrap();

        // Wrong count is rejected
        let result = SortReader::from_dir(dir.path(), Some(n + 5));
        let err = result.err().expect("expected error for mismatched chunk count");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let msg = err.to_string();
        assert!(msg.contains("chunk count mismatch"), "unexpected error: {msg}");
    }

    #[test]
    fn from_dir_skips_validation_when_none() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 100).unwrap();
        for i in 0u64..200 {
            writer.push(SortRecord { key: i, data: Box::from(i.to_le_bytes().as_slice()) }).unwrap();
        }
        let _ = writer.finish().unwrap();

        // None skips validation — always succeeds
        let reader = SortReader::from_dir(dir.path(), None);
        assert!(reader.is_ok());
    }

    #[test]
    fn flush_makes_chunk_count_accurate() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 10_000_000).unwrap();

        // Push some records (below chunk threshold)
        for i in 0u64..100 {
            writer.push(SortRecord { key: i, data: Box::from(i.to_le_bytes().as_slice()) }).unwrap();
        }
        assert_eq!(writer.chunk_count(), 0, "no auto-flush yet");

        // Explicit flush
        writer.flush().unwrap();
        assert_eq!(writer.chunk_count(), 1, "flush should create a chunk");

        // Double flush is a no-op
        writer.flush().unwrap();
        assert_eq!(writer.chunk_count(), 1, "flush on empty buffer is no-op");

        // finish() after flush doesn't add another chunk
        let count_before = writer.chunk_count();
        let _ = writer.finish().unwrap();
        // Can't check count after finish (consumed), but from_dir validates
        let reader = SortReader::from_dir(dir.path(), Some(count_before));
        assert!(reader.is_ok(), "count before finish should match disk");
    }

    #[test]
    fn flush_then_push_then_finish() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 10_000_000).unwrap();

        // First batch
        for i in 0u64..50 {
            writer.push(SortRecord { key: i, data: Box::from(i.to_le_bytes().as_slice()) }).unwrap();
        }
        writer.flush().unwrap();
        assert_eq!(writer.chunk_count(), 1);

        // Second batch
        for i in 50u64..100 {
            writer.push(SortRecord { key: i, data: Box::from(i.to_le_bytes().as_slice()) }).unwrap();
        }

        // finish() flushes the second batch
        let mut reader = writer.finish().unwrap();

        // All 100 records should be readable in sorted order
        let mut count = 0;
        while reader.next().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 100);
    }

    #[test]
    fn resume_keeps_checkpoint_chunks_and_deletes_leftovers() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Create 3 chunks on disk.
        let mut writer = SortWriter::new(dir.path(), 120).unwrap();
        for i in 0u64..300 {
            writer.push(SortRecord { key: i, data: Box::from(i.to_le_bytes().as_slice()) }).unwrap();
        }
        let total_chunks = writer.chunk_count();
        assert!(total_chunks >= 3, "expected >=3 chunks, got {total_chunks}");
        let _ = writer.finish().unwrap();

        // Resume from checkpoint that keeps only first 2 chunks.
        let resumed = SortWriter::resume(dir.path(), 120, 2).unwrap();
        assert_eq!(resumed.chunk_count(), 2);

        // chunk_0002.bin should have been deleted as stale leftover.
        assert!(!dir.path().join("chunk_0002.bin").exists());
        // checkpoint chunks must still exist.
        assert!(dir.path().join("chunk_0000.bin").exists());
        assert!(dir.path().join("chunk_0001.bin").exists());
    }

    #[test]
    fn resume_fails_if_required_chunk_missing() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Create two chunks, then remove chunk_0001 to simulate corrupted checkpoint state.
        let mut writer = SortWriter::new(dir.path(), 120).unwrap();
        for i in 0u64..200 {
            writer.push(SortRecord { key: i, data: Box::from(i.to_le_bytes().as_slice()) }).unwrap();
        }
        assert!(writer.chunk_count() >= 2);
        let _ = writer.finish().unwrap();
        std::fs::remove_file(dir.path().join("chunk_0001.bin")).unwrap();

        let err = SortWriter::resume(dir.path(), 120, 2).err().expect("resume should fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("missing chunk file"));
    }

    #[test]
    fn resume_allows_empty_checkpoint() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let writer = SortWriter::resume(dir.path(), 1024, 0).unwrap();
        assert_eq!(writer.chunk_count(), 0);
        let reader = writer.finish().unwrap();
        let keys = collect_keys(reader);
        assert!(keys.is_empty());
    }

    #[test]
    fn resume_start_chunk_zero_deletes_stale_chunks() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Seed stale chunks from a previous interrupted run.
        let mut seeded = SortWriter::new(dir.path(), 120).unwrap();
        for i in 0u64..200 {
            seeded.push(SortRecord { key: i, data: Box::from(i.to_le_bytes().as_slice()) }).unwrap();
        }
        assert!(seeded.chunk_count() >= 2, "expected stale chunks to exist");
        let _ = seeded.finish().unwrap();
        assert!(dir.path().join("chunk_0000.bin").exists());

        // Empty-checkpoint resume should prune all stale chunks and start clean.
        let resumed = SortWriter::resume(dir.path(), 120, 0).unwrap();
        assert_eq!(resumed.chunk_count(), 0);
        assert!(!dir.path().join("chunk_0000.bin").exists());
        assert!(!dir.path().join("chunk_0001.bin").exists());

        // New writes should restart naming from chunk_0000.bin.
        let mut resumed = resumed;
        resumed.push(SortRecord { key: 7, data: Box::from(7u64.to_le_bytes().as_slice()) }).unwrap();
        resumed.flush().unwrap();
        assert!(dir.path().join("chunk_0000.bin").exists());
    }

    #[test]
    fn adopt_chunk_files_updates_count_and_merges_records() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Main writer with one in-memory batch.
        let mut writer = SortWriter::new(dir.path(), 10_000_000).unwrap();
        writer.push(SortRecord { key: 40, data: Box::from(40u64.to_le_bytes().as_slice()) }).unwrap();
        writer.push(SortRecord { key: 20, data: Box::from(20u64.to_le_bytes().as_slice()) }).unwrap();

        // External chunk A.
        let ext_a = dir.path().join("external_a.bin");
        let mut recs_a = vec![
            SortRecord { key: 10, data: Box::from(10u64.to_le_bytes().as_slice()) },
            SortRecord { key: 30, data: Box::from(30u64.to_le_bytes().as_slice()) },
        ];
        write_sorted_chunk(&mut recs_a, &ext_a).unwrap();

        // External chunk B.
        let ext_b = dir.path().join("external_b.bin");
        let mut recs_b = vec![
            SortRecord { key: 5, data: Box::from(5u64.to_le_bytes().as_slice()) },
            SortRecord { key: 50, data: Box::from(50u64.to_le_bytes().as_slice()) },
        ];
        write_sorted_chunk(&mut recs_b, &ext_b).unwrap();

        writer.adopt_chunk_files(vec![ext_a, ext_b]);
        assert_eq!(writer.chunk_count(), 2, "adopted chunks should count immediately");

        // finish flushes writer buffer as chunk_0002.bin
        let reader = writer.finish().unwrap();
        let keys = collect_keys(reader);
        assert_eq!(keys, vec![5, 10, 20, 30, 40, 50]);
    }

    #[test]
    fn adopt_chunk_files_missing_or_corrupt_surfaces_error() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 10_000_000).unwrap();
        writer.push(SortRecord { key: 1, data: Box::from(1u64.to_le_bytes().as_slice()) }).unwrap();

        let missing = dir.path().join("does_not_exist.bin");
        writer.adopt_chunk_files(vec![missing]);
        let missing_err = writer.finish().err().expect("missing adopted chunk should fail");
        assert_eq!(missing_err.kind(), io::ErrorKind::NotFound);

        let mut writer2 = SortWriter::new(dir.path(), 10_000_000).unwrap();
        writer2.push(SortRecord { key: 2, data: Box::from(2u64.to_le_bytes().as_slice()) }).unwrap();

        // Corrupt chunk header (too short for u32 record count).
        let corrupt = dir.path().join("corrupt.bin");
        std::fs::write(&corrupt, [0xAA, 0xBB]).unwrap();
        writer2.adopt_chunk_files(vec![corrupt]);
        let corrupt_err = writer2.finish().err().expect("corrupt adopted chunk should fail");
        assert_eq!(corrupt_err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
