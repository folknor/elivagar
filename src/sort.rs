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

// ---------------------------------------------------------------------------
// Sort key helpers
// ---------------------------------------------------------------------------

/// Sort key: u64 encoding `(tile_id << 16) | (layer << 8) | priority`.
pub type SortKey = u64;

/// Build a sort key from tile id, layer index, and priority.
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
    pub data: Vec<u8>,
}

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
                    format!("Missing chunk file: {}", path.display()),
                ));
            }
            chunk_paths.push(path);
        }

        // Delete leftover chunks from a previous run of later phases
        let mut i = start_chunk;
        loop {
            let path = tmp_dir.join(format!("chunk_{i:04}.bin"));
            if path.exists() {
                fs::remove_file(&path)?;
                i += 1;
            } else {
                break;
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
        self.buffer_bytes += record.data.len() + 8; // 8 for the key
        self.buffer.push(record);
        if self.buffer_bytes >= self.chunk_size_bytes {
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// Flush the remaining buffer and return a `SortReader` for the k-way
    /// merge phase.
    pub fn finish(mut self) -> io::Result<SortReader> {
        if !self.buffer.is_empty() {
            self.flush_chunk()?;
        }
        SortReader::new(&self.chunk_paths)
    }

    /// Sort the in-memory buffer by key and write a chunk file to disk.
    ///
    /// Chunk file format (all little-endian):
    /// ```text
    /// u32 record_count
    /// For each record:
    ///   u64 key
    ///   u32 data_len
    ///   [u8; data_len] data
    /// ```
    #[allow(clippy::cast_possible_truncation)]
    fn flush_chunk(&mut self) -> io::Result<()> {
        self.buffer.sort_unstable_by_key(|r| r.key);

        let path = self.tmp_dir.join(format!("chunk_{:04}.bin", self.chunk_count));
        let file = File::create(&path)?;
        let mut writer = BufWriter::with_capacity(1 << 20, file);

        let count = self.buffer.len() as u32;
        writer.write_all(&count.to_le_bytes())?;

        for record in &self.buffer {
            writer.write_all(&record.key.to_le_bytes())?;
            let data_len = record.data.len() as u32;
            writer.write_all(&data_len.to_le_bytes())?;
            writer.write_all(&record.data)?;
        }

        writer.flush()?;

        self.chunk_paths.push(path);
        self.chunk_count += 1;
        self.buffer.clear();
        self.buffer_bytes = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ChunkReader — reads records sequentially from a single chunk file
// ---------------------------------------------------------------------------

struct ChunkReader {
    reader: BufReader<File>,
    remaining: u32,
}

impl ChunkReader {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::with_capacity(256 * 1024, file);

        let mut buf4 = [0u8; 4];
        reader.read_exact(&mut buf4)?;
        let remaining = u32::from_le_bytes(buf4);

        Ok(ChunkReader { reader, remaining })
    }

    // Allocates a Vec per record. A reusable buffer was considered but the heap
    // holds only k entries (1-4 chunks for Denmark, ~20 for planet) and records
    // vary in size, so a pool would often reallocate anyway. Not a bottleneck.
    fn read_record(&mut self) -> io::Result<Option<(SortKey, Vec<u8>)>> {
        if self.remaining == 0 {
            return Ok(None);
        }

        let mut buf8 = [0u8; 8];
        self.reader.read_exact(&mut buf8)?;
        let key = u64::from_le_bytes(buf8);

        let mut buf4 = [0u8; 4];
        self.reader.read_exact(&mut buf4)?;
        let data_len = u32::from_le_bytes(buf4);

        let mut data = vec![0u8; data_len as usize];
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
    data: Vec<u8>,
    chunk_idx: usize,
}

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
    pub fn from_dir(tmp_dir: &Path) -> io::Result<Self> {
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
    use std::path::PathBuf;

    /// Project-relative test tmp directory. Cleaned up after each test.
    fn test_tmp_dir(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".sort_test_tmp")
            .join(name)
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
        let dir = test_tmp_dir("single_chunk");
        drop(fs::remove_dir_all(&dir));

        // Use a large chunk size so everything fits in one chunk.
        let mut writer = SortWriter::new(&dir, 1_000_000).unwrap();

        // Push 1000 records with random-ish keys.
        let mut expected_keys: Vec<u64> = Vec::with_capacity(1000);
        for i in 0u64..1000 {
            // Simple scramble: reverse bits of i within a small range.
            let key = (i.wrapping_mul(7919)) ^ (i << 3);
            expected_keys.push(key);
            writer
                .push(SortRecord {
                    key,
                    data: i.to_le_bytes().to_vec(),
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


        drop(fs::remove_dir_all(&dir));
    }

    #[test]
    fn multi_chunk_sort() {
        let dir = test_tmp_dir("multi_chunk");
        drop(fs::remove_dir_all(&dir));

        // Very small chunk size forces multiple chunks.
        let mut writer = SortWriter::new(&dir, 100).unwrap();

        let mut expected_keys: Vec<u64> = Vec::with_capacity(500);
        for i in 0u64..500 {
            let key = (i.wrapping_mul(6271)) ^ (i << 5);
            expected_keys.push(key);
            writer
                .push(SortRecord {
                    key,
                    data: i.to_le_bytes().to_vec(),
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


        drop(fs::remove_dir_all(&dir));
    }

    #[test]
    fn empty_input() {
        let dir = test_tmp_dir("empty");
        drop(fs::remove_dir_all(&dir));

        let writer = SortWriter::new(&dir, 1_000_000).unwrap();
        let mut reader = writer.finish().unwrap();

        assert!(reader.next().unwrap().is_none());


        drop(fs::remove_dir_all(&dir));
    }

    #[test]
    fn duplicate_keys() {
        let dir = test_tmp_dir("dup_keys");
        drop(fs::remove_dir_all(&dir));

        // Very small chunk size to force multi-chunk even with few records.
        let mut writer = SortWriter::new(&dir, 50).unwrap();

        // 100 records all with the same key but different data.
        for i in 0u32..100 {
            writer
                .push(SortRecord {
                    key: 42,
                    data: i.to_le_bytes().to_vec(),
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


        drop(fs::remove_dir_all(&dir));
    }

    #[test]
    fn data_integrity() {
        let dir = test_tmp_dir("data_integrity");
        drop(fs::remove_dir_all(&dir));

        let mut writer = SortWriter::new(&dir, 200).unwrap();

        // Push records with keys and payload that can be verified.
        for i in 0u64..50 {
            let key = 50 - i; // descending keys
            let mut data = Vec::new();
            data.extend_from_slice(&key.to_le_bytes());
            data.extend_from_slice(b"payload_");
            data.extend_from_slice(&i.to_le_bytes());
            writer.push(SortRecord { key, data }).unwrap();
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


        drop(fs::remove_dir_all(&dir));
    }
}
