// External merge sort for tile feature records.
//
// Designed for planet-scale data (~100+ GB of sort records). Records are
// buffered in memory up to a configurable chunk size, flushed as sorted chunk
// files, then merged via a k-way merge using a binary heap.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};

use crate::debug::{WAIT, wait_span};
use crate::pipeline::emit::RecordTally;

use lz4_flex::frame::{FrameDecoder, FrameEncoder};

// ---------------------------------------------------------------------------
// K-way merge stats
// ---------------------------------------------------------------------------
//
// The merge is consumed lazily during assemble, so decompression work lands in
// assemble_reader_ns rather than the sort phase. These process-global atoms
// capture what that timing hides: `sort_merge_bytes` is the total decompressed
// record volume pulled through every chunk reader (per-reader local tally,
// flushed once on drop to keep the per-record path atomic-free), and
// `sort_merge_max_fanin` is the widest k-way merge (max concurrent chunk
// readers in one partition), which grows with dataset size and drives the
// per-record heap-compare cost. Flushed by emit_sort_counters after the reader
// is dropped. `sort_chunks` already reports the chunk count.
static SORT_MERGE_BYTES: AtomicU64 = AtomicU64::new(0);
static SORT_MERGE_MAX_FANIN: AtomicU64 = AtomicU64::new(0);

/// Flush accumulated k-way merge counters to the sidecar. Call after the
/// `SortReader` has been dropped (so all chunk readers have flushed their
/// tallies); a no-op when nothing was merged.
pub fn emit_sort_counters() {
    use std::sync::atomic::Ordering::Relaxed;
    let bytes = SORT_MERGE_BYTES.load(Relaxed);
    if bytes == 0 {
        return;
    }
    crate::debug::emit_counter_u64("sort_merge_bytes", bytes);
    crate::debug::emit_counter_u64("sort_merge_max_fanin", SORT_MERGE_MAX_FANIN.load(Relaxed));
}

/// Zoom level used to split each zoom block into ordered Hilbert ranges.
///
/// For z14 this makes each partition exactly one z6 Hilbert prefix, or 65,536
/// child tile ids. The previous equal-width split over the whole PMTiles id
/// space left Germany with one 8 GB hot partition, which erased the assemble
/// parallelism the partitioned path was meant to expose.
const PARTITION_SPLIT_Z: u8 = 6;

const TILE_ID_BASES: [u64; 16] = {
    let mut bases = [0u64; 16];
    let mut z = 0usize;
    while z < 16 {
        bases[z] = ((1u64 << (2 * z)) - 1) / 3;
        z += 1;
    }
    bases
};

const PARTITION_BASES: [usize; 16] = {
    let mut bases = [0usize; 16];
    let mut z = 0usize;
    let mut acc = 0usize;
    while z < 15 {
        bases[z] = acc;
        let split_z = if z < PARTITION_SPLIT_Z as usize {
            z
        } else {
            PARTITION_SPLIT_Z as usize
        };
        acc += 1usize << (2 * split_z);
        z += 1;
    }
    bases[15] = acc;
    bases
};

/// Number of ordered partition ids produced by the z6-calibrated scheme.
pub const SORT_PARTITIONS: usize = PARTITION_BASES[15];

const TILE_ID_LIMIT_EXCLUSIVE: u64 = TILE_ID_BASES[15];
const MULTI_CHUNK_MAGIC: &[u8; 8] = b"ELVGSRT1";

// ---------------------------------------------------------------------------
// Chunk compression selection
// ---------------------------------------------------------------------------

/// Compression algorithm for sort chunk files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChunkCompression {
    /// No compression (default). Fastest writes, largest files.
    #[default]
    None,
    /// LZ4 frame compression (lz4_flex). Good ratio, pure Rust.
    Lz4,
    /// Snappy frame compression (snap crate). Lower per-call overhead.
    Snappy,
}

// ---------------------------------------------------------------------------
// Chunk I/O abstraction - compressed reads
// ---------------------------------------------------------------------------

enum ChunkRead {
    Plain(BufReader<File>),
    Lz4(FrameDecoder<BufReader<File>>),
    Snappy(snap::read::FrameDecoder<BufReader<File>>),
}

impl Read for ChunkRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(r) => r.read(buf),
            Self::Lz4(r) => r.read(buf),
            Self::Snappy(r) => r.read(buf),
        }
    }
}

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

/// Extract zoom level from a tile_id. Uses the PMTiles base offset formula:
/// `base(z) = (4^z - 1) / 3`. Zoom is the largest z where `base(z) <= tile_id`.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn zoom_from_tile_id(tile_id: u64) -> u8 {
    let mut z: u8 = 14;
    while z > 0 && tile_id < TILE_ID_BASES[z as usize] {
        z -= 1;
    }
    z
}

/// Return the tile-id range partition for a sort key.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn partition_from_key(key: SortKey) -> usize {
    let tile_id = tile_id_from_key(key).min(TILE_ID_LIMIT_EXCLUSIVE - 1);
    let zoom = zoom_from_tile_id(tile_id);
    let local_id = tile_id - TILE_ID_BASES[zoom as usize];
    let split_z = zoom.min(PARTITION_SPLIT_Z);
    let shift = 2 * u32::from(zoom - split_z);
    let prefix = (local_id >> shift) as usize;
    let partition = PARTITION_BASES[zoom as usize] + prefix;
    debug_assert!(partition < SORT_PARTITIONS);
    partition
}

fn partition_start_tile_id(partition: usize) -> u64 {
    debug_assert!(partition < SORT_PARTITIONS);
    let mut zoom = 14usize;
    while partition < PARTITION_BASES[zoom] {
        zoom -= 1;
    }
    let split_z = zoom.min(PARTITION_SPLIT_Z as usize);
    let prefix = partition - PARTITION_BASES[zoom];
    TILE_ID_BASES[zoom] + ((prefix as u64) << (2 * (zoom - split_z)))
}

fn partition_next_key(partition: usize) -> SortKey {
    if partition + 1 >= SORT_PARTITIONS {
        SortKey::MAX
    } else {
        make_sort_key(partition_start_tile_id(partition + 1), 0, 0)
    }
}

fn chunk_path(tmp_dir: &Path, chunk_no: usize, partition: usize) -> PathBuf {
    tmp_dir.join(format!(
        "chunk_{chunk_no:04}_z{PARTITION_SPLIT_Z}p{partition:05}.bin"
    ))
}

fn multi_chunk_path(tmp_dir: &Path, chunk_no: usize) -> PathBuf {
    tmp_dir.join(format!("chunk_{chunk_no:04}_z{PARTITION_SPLIT_Z}m.bin"))
}

fn legacy_chunk_path(tmp_dir: &Path, chunk_no: usize) -> PathBuf {
    tmp_dir.join(format!("chunk_{chunk_no:04}.bin"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChunkFileKind {
    Legacy,
    Partition(usize),
    Multi,
}

fn parse_chunk_filename(path: &Path) -> Option<(usize, ChunkFileKind)> {
    let name = path.file_name()?.to_str()?;
    if !name.starts_with("chunk_") || !name.ends_with(".bin") {
        return None;
    }
    let stem = &name[..name.len() - 4];
    let body = stem.strip_prefix("chunk_")?;
    if let Some((id, suffix)) = body.split_once("_z") {
        let chunk_no = id.parse().ok()?;
        if let Some(split_z) = suffix.strip_suffix('m') {
            let split_z: u8 = split_z.parse().ok()?;
            if split_z == PARTITION_SPLIT_Z {
                return Some((chunk_no, ChunkFileKind::Multi));
            }
            return Some((chunk_no, ChunkFileKind::Legacy));
        }
        let (split_z, partition) = suffix.split_once('p')?;
        let split_z: u8 = split_z.parse().ok()?;
        let part: usize = partition.parse().ok()?;
        if split_z == PARTITION_SPLIT_Z && part < SORT_PARTITIONS {
            return Some((chunk_no, ChunkFileKind::Partition(part)));
        }
        return Some((chunk_no, ChunkFileKind::Legacy));
    }
    if let Some((id, _partition)) = body.split_once("_p") {
        // First-cut P3 chunks used a different partition numbering scheme.
        // Keep them visible for checkpoint cleanup and legacy merge fallback,
        // but do not treat the suffix as a current partition id.
        let chunk_no = id.parse().ok()?;
        return Some((chunk_no, ChunkFileKind::Legacy));
    }
    let chunk_no = body.parse().ok()?;
    Some((chunk_no, ChunkFileKind::Legacy))
}

type ChunkScanEntry = (usize, ChunkFileKind, PathBuf);
type ChunkById = Vec<Option<(ChunkFileKind, PathBuf)>>;

fn scan_chunk_files(tmp_dir: &Path) -> io::Result<Vec<ChunkScanEntry>> {
    let mut out = Vec::new();
    match fs::read_dir(tmp_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if let Some((chunk_no, kind)) = parse_chunk_filename(&path) {
                    out.push((chunk_no, kind, path));
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    out.sort_unstable_by_key(|(chunk_no, _, _)| *chunk_no);
    Ok(out)
}

fn chunk_files_by_id(tmp_dir: &Path) -> io::Result<ChunkById> {
    let scanned = scan_chunk_files(tmp_dir)?;
    let max_id = scanned
        .iter()
        .map(|(chunk_no, _, _)| *chunk_no)
        .max()
        .map_or(0, |id| id + 1);
    let mut by_id = vec![None; max_id];
    for (chunk_no, kind, path) in scanned {
        if by_id[chunk_no].is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate chunk id {chunk_no} in {}", tmp_dir.display()),
            ));
        }
        by_id[chunk_no] = Some((kind, path));
    }
    Ok(by_id)
}

// ---------------------------------------------------------------------------
// SortRecord
// ---------------------------------------------------------------------------

/// A record in the sort buffer: a sort key plus opaque payload bytes.
///
/// `data` must be owned because records are serialized to chunk files on
/// disk and deserialized during k-way merge. Hot producers that already write
/// direct chunks can use `write_sorted_payload_chunk` to sort arena indexes
/// without changing the chunk file format or the merge reader ownership model.
pub struct SortRecord {
    pub key: SortKey,
    pub data: Box<[u8]>,
}
const _: () = assert!(std::mem::size_of::<SortRecord>() == 24);

pub type PayloadRecord = (SortKey, usize, usize);

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
    compression: ChunkCompression,
    total_records: u64,
    total_record_bytes: u64,
    /// Per-layer record counts and bytes (indexed by layer_from_key).
    layer_records: [u64; 32],
    layer_bytes: [u64; 32],
    /// Per-layer-per-zoom record counts. Index: layer * 15 + zoom.
    layer_zoom_records: Box<[u64; 32 * 15]>,
    /// Per-layer-per-zoom payload bytes. Index: layer * 15 + zoom.
    layer_zoom_bytes: Box<[u64; 32 * 15]>,
    /// Shared chunk-number allocator, active only while a concurrent producer
    /// (the way-phase drain) writes chunks into the same directory from other
    /// threads. When set, `flush_chunk` draws chunk numbers from this atomic so
    /// they never collide with the numbers those producers allocate; `chunk_count`
    /// is resynced from it on detach. `None` restores the plain self-counted path.
    chunk_counter: Option<Arc<AtomicUsize>>,
}

impl SortWriter {
    /// Create a new sort writer. `chunk_size_bytes` is the target memory
    /// budget per chunk (typically ~1 GB).
    pub fn new(
        tmp_dir: &Path,
        chunk_size_bytes: usize,
        compression: ChunkCompression,
    ) -> io::Result<Self> {
        fs::create_dir_all(tmp_dir)?;
        Ok(SortWriter {
            tmp_dir: tmp_dir.to_path_buf(),
            buffer: Vec::new(),
            buffer_bytes: 0,
            chunk_size_bytes,
            chunk_paths: Vec::new(),
            chunk_count: 0,
            compression,
            total_records: 0,
            total_record_bytes: 0,
            layer_records: [0; 32],
            layer_bytes: [0; 32],
            layer_zoom_records: Box::new([0; 32 * 15]),
            layer_zoom_bytes: Box::new([0; 32 * 15]),
            chunk_counter: None,
        })
    }

    /// Resume a sort writer with existing chunk files in the tmp dir.
    /// `start_chunk` is the number of chunks to keep (from a previous phase);
    /// any chunks beyond that are deleted (leftovers from a previous run).
    pub fn resume(
        tmp_dir: &Path,
        chunk_size_bytes: usize,
        start_chunk: usize,
        compression: ChunkCompression,
    ) -> io::Result<Self> {
        let mut chunk_paths: Vec<PathBuf> = Vec::with_capacity(start_chunk);
        let mut by_id = chunk_files_by_id(tmp_dir)?;
        for i in 0..start_chunk {
            match by_id.get_mut(i).and_then(Option::take) {
                Some((_, path)) => chunk_paths.push(path),
                None => {
                    let path = legacy_chunk_path(tmp_dir, i);
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("missing chunk file: {}", path.display()),
                    ));
                }
            }
        }

        // Delete leftover chunks from a previous run. Scan beyond gaps to
        // catch stale chunks that would otherwise contaminate a later sort.
        for (i, entry) in by_id.into_iter().enumerate() {
            if i >= start_chunk
                && let Some((_, path)) = entry
            {
                fs::remove_file(path)?;
            }
        }

        Ok(SortWriter {
            tmp_dir: tmp_dir.to_path_buf(),
            buffer: Vec::new(),
            buffer_bytes: 0,
            chunk_size_bytes,
            chunk_paths,
            chunk_count: start_chunk,
            compression,
            total_records: 0,
            total_record_bytes: 0,
            layer_records: [0; 32],
            layer_bytes: [0; 32],
            layer_zoom_records: Box::new([0; 32 * 15]),
            layer_zoom_bytes: Box::new([0; 32 * 15]),
            chunk_counter: None,
        })
    }

    /// Number of chunk files written so far (including adopted ones from resume).
    pub fn chunk_count(&self) -> usize {
        self.chunk_count
    }

    /// Total sort records pushed (across all chunks + current buffer).
    pub fn total_records(&self) -> u64 {
        self.total_records
    }

    /// Total payload bytes pushed (sum of record.data.len()).
    pub fn total_record_bytes(&self) -> u64 {
        self.total_record_bytes
    }

    /// Per-layer record counts (indexed by layer id, 0..26).
    pub fn layer_records(&self) -> &[u64; 32] {
        &self.layer_records
    }

    /// Per-layer payload bytes (indexed by layer id, 0..26).
    pub fn layer_bytes(&self) -> &[u64; 32] {
        &self.layer_bytes
    }

    /// Per-layer-per-zoom record counts. Index: `layer * 15 + zoom`.
    pub fn layer_zoom_records(&self) -> &[u64; 32 * 15] {
        &self.layer_zoom_records
    }

    /// Per-layer-per-zoom payload bytes. Index: `layer * 15 + zoom`.
    pub fn layer_zoom_bytes(&self) -> &[u64; 32 * 15] {
        &self.layer_zoom_bytes
    }

    /// Add a record to the buffer. If the buffer exceeds `chunk_size_bytes`,
    /// the current buffer is sorted and flushed to a chunk file on disk.
    pub fn push(&mut self, record: SortRecord) -> io::Result<()> {
        let data_len = record.data.len();
        let layer = layer_from_key(record.key) as usize;
        self.buffer_bytes += data_len + std::mem::size_of::<SortRecord>();
        self.total_records += 1;
        self.total_record_bytes += data_len as u64;
        if layer < 32 {
            self.layer_records[layer] += 1;
            self.layer_bytes[layer] += data_len as u64;
            let tile_id = tile_id_from_key(record.key);
            let zoom = zoom_from_tile_id(tile_id) as usize;
            if zoom < 15 {
                let idx = layer * 15 + zoom;
                self.layer_zoom_records[idx] += 1;
                self.layer_zoom_bytes[idx] += data_len as u64;
            }
        }
        self.buffer.push(record);
        if self.buffer_bytes >= self.chunk_size_bytes {
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// Add a record to the buffer without updating statistics.
    ///
    /// Arena producers merge their own tally before pushing leftover tail records
    /// through this path, so using `push` would double-count those tails.
    pub(crate) fn push_untracked(&mut self, record: SortRecord) -> io::Result<()> {
        self.buffer_bytes += record.data.len() + std::mem::size_of::<SortRecord>();
        self.buffer.push(record);
        if self.buffer_bytes >= self.chunk_size_bytes {
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// Install a shared chunk-number allocator for the window during which
    /// other threads write chunk files into this writer's directory concurrently
    /// (the way-phase tasks). Both this writer's `flush_chunk` and those producers
    /// must `fetch_add` the same atomic so no two chunks claim the same number.
    /// The counter MUST be initialized to the current `chunk_count()` by the caller.
    pub(crate) fn attach_chunk_counter(&mut self, counter: Arc<AtomicUsize>) {
        self.chunk_counter = Some(counter);
    }

    /// Remove the shared allocator and resync `chunk_count` from its final value,
    /// so subsequent phases (ocean, relations) and `from_dir` see the true total.
    /// Call only once every concurrent producer has stopped allocating.
    pub(crate) fn detach_chunk_counter(&mut self) {
        if let Some(counter) = self.chunk_counter.take() {
            self.chunk_count = counter.load(std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(crate) fn merge_tally(&mut self, tally: &RecordTally) {
        self.total_records += tally.total_records;
        self.total_record_bytes += tally.total_record_bytes;
        for i in 0..32 {
            self.layer_records[i] += tally.layer_records[i];
            self.layer_bytes[i] += tally.layer_bytes[i];
        }
        for i in 0..(32 * 15) {
            self.layer_zoom_records[i] += tally.layer_zoom_records[i];
            self.layer_zoom_bytes[i] += tally.layer_zoom_bytes[i];
        }
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
        SortReader::new(&self.chunk_paths, self.compression)
    }

    /// Read accessor for the temporary directory.
    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    /// Read accessor for the chunk size budget.
    pub fn chunk_size_bytes(&self) -> usize {
        self.chunk_size_bytes
    }

    /// Compression algorithm for chunk files.
    pub fn compression(&self) -> ChunkCompression {
        self.compression
    }

    /// Adopt externally-written chunk files (e.g., from parallel ocean processing).
    /// Files must be in standard chunk format (sorted records). The chunk_count is
    /// updated so that subsequent flushes and `from_dir` scans remain consistent.
    pub fn adopt_chunk_files(&mut self, paths: Vec<PathBuf>) {
        self.chunk_count += paths.len();
        self.chunk_paths.extend(paths);
    }

    /// Sort the in-memory buffer by key and write a chunk file to disk.
    fn flush_chunk(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer.sort_unstable_by_key(|r| r.key);
        let mut paths = Vec::new();
        if self.compression == ChunkCompression::None {
            let chunk_no = match &self.chunk_counter {
                Some(counter) => counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                None => self.chunk_count + paths.len(),
            };
            let path =
                write_uncompressed_partitioned_sort_chunk(&self.buffer, &self.tmp_dir, chunk_no)?;
            paths.push(path);
        } else {
            let ranges = partition_ranges_by_key(self.buffer.len(), |idx| self.buffer[idx].key);
            for range in ranges {
                let chunk_no = match &self.chunk_counter {
                    Some(counter) => counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    None => self.chunk_count + paths.len(),
                };
                let path = chunk_path(&self.tmp_dir, chunk_no, range.partition);
                write_chunk_records_presorted(
                    &self.buffer[range.start..range.end],
                    &path,
                    self.compression,
                )?;
                paths.push(path);
            }
        }

        if self.chunk_counter.is_none() {
            self.chunk_count += paths.len();
        }
        self.chunk_paths.extend(paths);
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
pub fn write_sorted_chunk(
    records: &mut [SortRecord],
    path: &Path,
    compression: ChunkCompression,
) -> io::Result<()> {
    records.sort_unstable_by_key(|r| r.key);
    write_chunk_records_presorted(records, path, compression)
}

#[allow(clippy::cast_possible_truncation)]
fn write_chunk_records_presorted(
    records: &[SortRecord],
    path: &Path,
    compression: ChunkCompression,
) -> io::Result<()> {
    let _wait = wait_span(&WAIT.sort_chunk_write);
    // Record count as u32. Safe: 1 GB chunk budget yields max ~48.8M records
    // (minimum 22 bytes each), 88x below u32::MAX.
    let count = records.len() as u32;

    if compression != ChunkCompression::None {
        // Pre-serialize all records into a contiguous buffer, then compress
        // in bulk. Avoids millions of small write_all calls through the
        // frame encoder, which caused a ~25s regression on Germany with lz4.
        let serialized_size =
            4 + records.len() * 12 + records.iter().map(|r| r.data.len()).sum::<usize>();
        let mut serialized = Vec::with_capacity(serialized_size);
        serialized.extend_from_slice(&count.to_le_bytes());
        for record in records {
            serialized.extend_from_slice(&record.key.to_le_bytes());
            let data_len = record.data.len() as u32;
            serialized.extend_from_slice(&data_len.to_le_bytes());
            serialized.extend_from_slice(&record.data);
        }

        let file = File::create(path)?;
        let buf = BufWriter::with_capacity(1 << 20, file);
        match compression {
            ChunkCompression::None => unreachable!(),
            ChunkCompression::Lz4 => {
                let mut encoder = FrameEncoder::new(buf);
                encoder.write_all(&serialized)?;
                encoder.finish().map_err(io::Error::other)?;
            }
            ChunkCompression::Snappy => {
                let mut encoder = snap::write::FrameEncoder::new(buf);
                encoder.write_all(&serialized)?;
                encoder.flush()?;
            }
        }
    } else {
        let file = File::create(path)?;
        let mut writer = BufWriter::with_capacity(1 << 20, file);
        writer.write_all(&count.to_le_bytes())?;
        for record in records {
            writer.write_all(&record.key.to_le_bytes())?;
            let data_len = record.data.len() as u32;
            writer.write_all(&data_len.to_le_bytes())?;
            writer.write_all(&record.data)?;
        }
        writer.flush()?;
    }

    Ok(())
}

#[derive(Clone, Copy)]
struct PartitionRange {
    partition: usize,
    start: usize,
    end: usize,
}

fn partition_ranges_by_key(
    len: usize,
    mut key_at: impl FnMut(usize) -> SortKey,
) -> Vec<PartitionRange> {
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < len {
        let partition = partition_from_key(key_at(start));
        let next_key = partition_next_key(partition);
        let mut end = start + 1;
        while end < len && key_at(end) < next_key {
            end += 1;
        }
        ranges.push(PartitionRange {
            partition,
            start,
            end,
        });
        start = end;
    }
    ranges
}

#[inline]
fn sort_record_bytes(record: &SortRecord) -> u64 {
    12 + record.data.len() as u64
}

#[inline]
fn payload_record_bytes(record: PayloadRecord) -> u64 {
    12 + record.2 as u64
}

#[allow(clippy::cast_possible_truncation)]
fn write_multi_sort_chunk(
    records: &[SortRecord],
    ranges: &[PartitionRange],
    path: &Path,
) -> io::Result<()> {
    let _wait = wait_span(&WAIT.sort_chunk_write);
    let mut sections = Vec::with_capacity(ranges.len());
    let mut offset = 8 + 4 + (ranges.len() as u64 * 16);
    for range in ranges {
        let byte_len = records[range.start..range.end]
            .iter()
            .map(sort_record_bytes)
            .sum::<u64>();
        sections.push((range.partition, range.end - range.start, offset));
        offset += byte_len;
    }

    let file = File::create(path)?;
    let mut writer = BufWriter::with_capacity(1 << 20, file);
    writer.write_all(MULTI_CHUNK_MAGIC)?;
    writer.write_all(&(sections.len() as u32).to_le_bytes())?;
    for &(partition, count, offset) in &sections {
        writer.write_all(&(partition as u32).to_le_bytes())?;
        writer.write_all(&(count as u32).to_le_bytes())?;
        writer.write_all(&offset.to_le_bytes())?;
    }
    for range in ranges {
        for record in &records[range.start..range.end] {
            writer.write_all(&record.key.to_le_bytes())?;
            let data_len = record.data.len() as u32;
            writer.write_all(&data_len.to_le_bytes())?;
            writer.write_all(&record.data)?;
        }
    }
    writer.flush()
}

#[allow(clippy::cast_possible_truncation)]
fn write_multi_payload_chunk(
    records: &[PayloadRecord],
    payload: &[u8],
    ranges: &[PartitionRange],
    path: &Path,
) -> io::Result<()> {
    let _wait = wait_span(&WAIT.sort_chunk_write);
    let mut sections = Vec::with_capacity(ranges.len());
    let mut offset = 8 + 4 + (ranges.len() as u64 * 16);
    for range in ranges {
        let byte_len = records[range.start..range.end]
            .iter()
            .copied()
            .map(payload_record_bytes)
            .sum::<u64>();
        sections.push((range.partition, range.end - range.start, offset));
        offset += byte_len;
    }

    let file = File::create(path)?;
    let mut writer = BufWriter::with_capacity(1 << 20, file);
    writer.write_all(MULTI_CHUNK_MAGIC)?;
    writer.write_all(&(sections.len() as u32).to_le_bytes())?;
    for &(partition, count, offset) in &sections {
        writer.write_all(&(partition as u32).to_le_bytes())?;
        writer.write_all(&(count as u32).to_le_bytes())?;
        writer.write_all(&offset.to_le_bytes())?;
    }
    for range in ranges {
        for &(key, offset, len) in &records[range.start..range.end] {
            writer.write_all(&key.to_le_bytes())?;
            let data_len = len as u32;
            writer.write_all(&data_len.to_le_bytes())?;
            writer.write_all(&payload[offset..offset + len])?;
        }
    }
    writer.flush()
}

fn write_uncompressed_partitioned_sort_chunk(
    records: &[SortRecord],
    tmp_dir: &Path,
    chunk_no: usize,
) -> io::Result<PathBuf> {
    let ranges = partition_ranges_by_key(records.len(), |idx| records[idx].key);
    if ranges.len() == 1 {
        let path = chunk_path(tmp_dir, chunk_no, ranges[0].partition);
        write_chunk_records_presorted(records, &path, ChunkCompression::None)?;
        Ok(path)
    } else {
        let path = multi_chunk_path(tmp_dir, chunk_no);
        write_multi_sort_chunk(records, &ranges, &path)?;
        Ok(path)
    }
}

fn write_uncompressed_partitioned_payload_chunk(
    records: &[PayloadRecord],
    payload: &[u8],
    tmp_dir: &Path,
    chunk_no: usize,
) -> io::Result<PathBuf> {
    let ranges = partition_ranges_by_key(records.len(), |idx| records[idx].0);
    if ranges.len() == 1 {
        let path = chunk_path(tmp_dir, chunk_no, ranges[0].partition);
        write_payload_chunk_records_presorted(records, payload, &path, ChunkCompression::None)?;
        Ok(path)
    } else {
        let path = multi_chunk_path(tmp_dir, chunk_no);
        write_multi_payload_chunk(records, payload, &ranges, &path)?;
        Ok(path)
    }
}

/// Write arena-backed records as a sorted chunk file in the standard format.
///
/// `records` entries are `(key, offset, len)` into `payload`. Only this in-memory
/// staging differs from `write_sorted_chunk`; the bytes on disk are identical.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
pub fn write_sorted_payload_chunk(
    records: &mut [PayloadRecord],
    payload: &[u8],
    path: &Path,
    compression: ChunkCompression,
) -> io::Result<()> {
    records.sort_unstable_by_key(|r| r.0);
    write_payload_chunk_records_presorted(records, payload, path, compression)
}

#[allow(clippy::cast_possible_truncation)]
fn write_payload_chunk_records_presorted(
    records: &[PayloadRecord],
    payload: &[u8],
    path: &Path,
    compression: ChunkCompression,
) -> io::Result<()> {
    let _wait = wait_span(&WAIT.sort_chunk_write);
    let count = records.len() as u32;

    if compression != ChunkCompression::None {
        let serialized_size = 4 + records.len() * 12 + records.iter().map(|r| r.2).sum::<usize>();
        let mut serialized = Vec::with_capacity(serialized_size);
        serialized.extend_from_slice(&count.to_le_bytes());
        for &(key, offset, len) in records {
            serialized.extend_from_slice(&key.to_le_bytes());
            let data_len = len as u32;
            serialized.extend_from_slice(&data_len.to_le_bytes());
            serialized.extend_from_slice(&payload[offset..offset + len]);
        }

        let file = File::create(path)?;
        let buf = BufWriter::with_capacity(1 << 20, file);
        match compression {
            ChunkCompression::None => unreachable!(),
            ChunkCompression::Lz4 => {
                let mut encoder = FrameEncoder::new(buf);
                encoder.write_all(&serialized)?;
                encoder.finish().map_err(io::Error::other)?;
            }
            ChunkCompression::Snappy => {
                let mut encoder = snap::write::FrameEncoder::new(buf);
                encoder.write_all(&serialized)?;
                encoder.flush()?;
            }
        }
    } else {
        let file = File::create(path)?;
        let mut writer = BufWriter::with_capacity(1 << 20, file);
        writer.write_all(&count.to_le_bytes())?;
        for &(key, offset, len) in records {
            writer.write_all(&key.to_le_bytes())?;
            let data_len = len as u32;
            writer.write_all(&data_len.to_le_bytes())?;
            writer.write_all(&payload[offset..offset + len])?;
        }
        writer.flush()?;
    }

    Ok(())
}

/// Write arena-backed records as partition-pure sorted chunk files.
///
/// The caller provides the shared chunk id allocator used by direct producers.
/// Returned paths are suitable for `SortWriter::adopt_chunk_files`.
#[hotpath::measure]
pub fn write_partitioned_payload_chunks(
    records: &mut [PayloadRecord],
    payload: &[u8],
    tmp_dir: &Path,
    chunk_id: &AtomicUsize,
    compression: ChunkCompression,
) -> io::Result<Vec<PathBuf>> {
    if records.is_empty() {
        return Ok(Vec::new());
    }
    records.sort_unstable_by_key(|r| r.0);
    if compression == ChunkCompression::None {
        let id = chunk_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = write_uncompressed_partitioned_payload_chunk(records, payload, tmp_dir, id)?;
        return Ok(vec![path]);
    }

    let ranges = partition_ranges_by_key(records.len(), |idx| records[idx].0);
    let mut paths = Vec::new();
    for range in ranges {
        let id = chunk_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = chunk_path(tmp_dir, id, range.partition);
        write_payload_chunk_records_presorted(
            &records[range.start..range.end],
            payload,
            &path,
            compression,
        )?;
        paths.push(path);
    }
    Ok(paths)
}

// ---------------------------------------------------------------------------
// ChunkReader - reads records sequentially from a single chunk file
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct MultiChunkSection {
    partition: usize,
    offset: u64,
    count: u32,
}

#[derive(Clone, Debug)]
struct SortPartitionSource {
    path: PathBuf,
    section: Option<MultiChunkSection>,
}

impl SortPartitionSource {
    fn whole(path: PathBuf) -> Self {
        Self {
            path,
            section: None,
        }
    }

    fn section(path: PathBuf, section: MultiChunkSection) -> Self {
        Self {
            path,
            section: Some(section),
        }
    }
}

fn read_multi_chunk_sections(path: &Path) -> io::Result<Vec<MultiChunkSection>> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != MULTI_CHUNK_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid multi-partition chunk magic in {}", path.display()),
        ));
    }

    let mut buf4 = [0u8; 4];
    file.read_exact(&mut buf4)?;
    let section_count = usize::try_from(u32::from_le_bytes(buf4)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("section count does not fit usize in {}", path.display()),
        )
    })?;
    let mut sections = Vec::with_capacity(section_count);
    for _ in 0..section_count {
        file.read_exact(&mut buf4)?;
        let partition = usize::try_from(u32::from_le_bytes(buf4)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("partition id does not fit usize in {}", path.display()),
            )
        })?;
        if partition >= SORT_PARTITIONS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid partition {partition} in {}", path.display()),
            ));
        }

        file.read_exact(&mut buf4)?;
        let count = u32::from_le_bytes(buf4);

        let mut buf8 = [0u8; 8];
        file.read_exact(&mut buf8)?;
        let offset = u64::from_le_bytes(buf8);

        sections.push(MultiChunkSection {
            partition,
            offset,
            count,
        });
    }
    Ok(sections)
}

struct ChunkReader {
    reader: ChunkRead,
    remaining: u32,
    // Decompressed record bytes read from this chunk, flushed to
    // SORT_MERGE_BYTES on drop so the per-record path stays atomic-free.
    bytes_read: u64,
}

impl Drop for ChunkReader {
    fn drop(&mut self) {
        SORT_MERGE_BYTES.fetch_add(self.bytes_read, std::sync::atomic::Ordering::Relaxed);
    }
}

impl ChunkReader {
    fn open(path: &Path, compression: ChunkCompression) -> io::Result<Self> {
        let file = File::open(path)?;
        let buf = BufReader::with_capacity(256 * 1024, file);
        let mut reader = match compression {
            ChunkCompression::None => ChunkRead::Plain(buf),
            ChunkCompression::Lz4 => ChunkRead::Lz4(FrameDecoder::new(buf)),
            ChunkCompression::Snappy => ChunkRead::Snappy(snap::read::FrameDecoder::new(buf)),
        };

        let mut buf4 = [0u8; 4];
        reader.read_exact(&mut buf4)?;
        let remaining = u32::from_le_bytes(buf4);

        Ok(ChunkReader {
            reader,
            remaining,
            bytes_read: 0,
        })
    }

    fn open_source(
        source: &SortPartitionSource,
        compression: ChunkCompression,
    ) -> io::Result<Self> {
        match &source.section {
            None => Self::open(&source.path, compression),
            Some(section) => {
                if compression != ChunkCompression::None {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "compressed multi-partition chunk section is not supported: {}",
                            source.path.display()
                        ),
                    ));
                }
                let mut file = File::open(&source.path)?;
                file.seek(SeekFrom::Start(section.offset))?;
                let buf = BufReader::with_capacity(256 * 1024, file);
                Ok(ChunkReader {
                    reader: ChunkRead::Plain(buf),
                    remaining: section.count,
                    bytes_read: 0,
                })
            }
        }
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
        self.bytes_read += 12 + u64::from(data_len);
        Ok(Some((key, data)))
    }
}

// ---------------------------------------------------------------------------
// HeapEntry - element in the merge heap
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
// SortReader - k-way merge of sorted chunk files
// ---------------------------------------------------------------------------

struct PartitionMergeReader {
    chunk_readers: Vec<ChunkReader>,
    heap: BinaryHeap<HeapEntry>,
}

impl PartitionMergeReader {
    fn new(sources: &[SortPartitionSource], compression: ChunkCompression) -> io::Result<Self> {
        let mut chunk_readers = Vec::with_capacity(sources.len());
        let mut heap = BinaryHeap::with_capacity(sources.len());

        for (idx, source) in sources.iter().enumerate() {
            let mut cr = ChunkReader::open_source(source, compression)?;
            if let Some((key, data)) = cr.read_record()? {
                heap.push(HeapEntry {
                    key,
                    data,
                    chunk_idx: idx,
                });
            }
            chunk_readers.push(cr);
        }

        SORT_MERGE_MAX_FANIN.fetch_max(
            u64::try_from(chunk_readers.len()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );

        Ok(Self {
            chunk_readers,
            heap,
        })
    }

    fn new_whole_paths(chunk_paths: &[PathBuf], compression: ChunkCompression) -> io::Result<Self> {
        let sources: Vec<SortPartitionSource> = chunk_paths
            .iter()
            .cloned()
            .map(SortPartitionSource::whole)
            .collect();
        Self::new(&sources, compression)
    }

    fn next(&mut self) -> io::Result<Option<SortRecord>> {
        let entry = match self.heap.pop() {
            Some(e) => e,
            None => return Ok(None),
        };

        let idx = entry.chunk_idx;
        let result = SortRecord {
            key: entry.key,
            data: entry.data,
        };

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

/// One tile-id range partition of sort chunk files.
pub struct SortPartition {
    pub index: usize,
    sources: Vec<SortPartitionSource>,
}

/// A reader for one partition's chunk files.
pub struct SortPartitionReader {
    inner: PartitionMergeReader,
}

impl SortPartitionReader {
    pub fn open(partition: &SortPartition, compression: ChunkCompression) -> io::Result<Self> {
        Ok(Self {
            inner: PartitionMergeReader::new(&partition.sources, compression)?,
        })
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> io::Result<Option<SortRecord>> {
        self.inner.next()
    }
}

enum SortReaderMode {
    Partitioned {
        partitions: Vec<Vec<SortPartitionSource>>,
        next_partition: usize,
        current: Option<PartitionMergeReader>,
    },
    Legacy(PartitionMergeReader),
}

/// Reads sorted records from multiple chunk files using partition-aware
/// per-range merges when all files carry partition suffixes.
pub struct SortReader {
    mode: SortReaderMode,
    compression: ChunkCompression,
}

impl SortReader {
    /// Open all chunk files found in a directory and create a merge reader.
    ///
    /// `expected_chunks`: if `Some(n)`, verifies exactly `n` contiguous chunk files exist.
    /// Detects stale leftover chunks from a previous run that could silently contaminate
    /// the merge. Pass `None` to skip validation (not recommended for `--skip-to sort`).
    pub fn from_dir(
        tmp_dir: &Path,
        expected_chunks: Option<usize>,
        compression: ChunkCompression,
    ) -> io::Result<Self> {
        let by_id = chunk_files_by_id(tmp_dir)?;
        if let Some(expected) = expected_chunks {
            for i in 0..expected {
                if by_id.get(i).and_then(Option::as_ref).is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "chunk count mismatch: missing chunk {i} but checkpoint expects {expected}. \
                             Stale chunks from a previous run may be present - \
                             run a full pipeline (without --skip-to) to regenerate.",
                        ),
                    ));
                }
            }
            if by_id.len() > expected && by_id[expected..].iter().any(Option::is_some) {
                let found = by_id.iter().filter(|entry| entry.is_some()).count();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "chunk count mismatch: found {found} but checkpoint expects {expected}. \
                         Stale chunks from a previous run may be present - \
                         run a full pipeline (without --skip-to) to regenerate.",
                    ),
                ));
            }
        }
        let chunk_paths: Vec<PathBuf> = by_id
            .iter()
            .filter_map(|entry| entry.as_ref().map(|(_, path)| path.clone()))
            .collect();
        Self::new(&chunk_paths, compression)
    }

    /// Open all chunk files and prime the merge heap with the first record
    /// from each chunk.
    fn new(chunk_paths: &[PathBuf], compression: ChunkCompression) -> io::Result<Self> {
        let mut partitions = vec![Vec::new(); SORT_PARTITIONS];
        let mut saw_partitioned = false;
        let mut saw_multi = false;
        let mut saw_legacy = false;
        for path in chunk_paths {
            match parse_chunk_filename(path).map(|(_, kind)| kind) {
                Some(ChunkFileKind::Partition(partition)) => {
                    saw_partitioned = true;
                    partitions[partition].push(SortPartitionSource::whole(path.clone()));
                }
                Some(ChunkFileKind::Multi) => {
                    saw_partitioned = true;
                    saw_multi = true;
                    for section in read_multi_chunk_sections(path)? {
                        if section.count > 0 {
                            let partition = section.partition;
                            partitions[partition]
                                .push(SortPartitionSource::section(path.clone(), section));
                        }
                    }
                }
                Some(ChunkFileKind::Legacy) | None => {
                    saw_legacy = true;
                }
            }
        }
        if saw_legacy && saw_multi {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cannot mix legacy chunks with indexed multi-partition chunks",
            ));
        }
        if saw_partitioned && !saw_legacy {
            return Ok(Self {
                mode: SortReaderMode::Partitioned {
                    partitions,
                    next_partition: 0,
                    current: None,
                },
                compression,
            });
        }
        Ok(Self {
            mode: SortReaderMode::Legacy(PartitionMergeReader::new_whole_paths(
                chunk_paths,
                compression,
            )?),
            compression,
        })
    }

    /// Take partition groups for partition-level parallel assembly.
    ///
    /// Returns `None` when the reader is in legacy mode because at least one
    /// chunk file did not carry a partition suffix.
    pub fn take_partitions(&mut self) -> Option<Vec<SortPartition>> {
        match &mut self.mode {
            SortReaderMode::Partitioned { partitions, .. } => {
                let mut taken = Vec::new();
                for (index, paths) in std::mem::take(partitions).into_iter().enumerate() {
                    if !paths.is_empty() {
                        taken.push(SortPartition {
                            index,
                            sources: paths,
                        });
                    }
                }
                Some(taken)
            }
            SortReaderMode::Legacy(_) => None,
        }
    }

    /// Return the next record in globally sorted order, or `None` when all
    /// records have been consumed.
    ///
    /// Named `next` for clarity, but can't implement `Iterator` because iteration
    /// is fallible (`io::Result`). The `fallible-iterator` crate isn't worth the dep.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> io::Result<Option<SortRecord>> {
        let compression = self.compression;
        match &mut self.mode {
            SortReaderMode::Legacy(reader) => reader.next(),
            SortReaderMode::Partitioned {
                partitions,
                next_partition,
                current,
            } => loop {
                if let Some(reader) = current
                    && let Some(record) = reader.next()?
                {
                    return Ok(Some(record));
                }
                *current = None;
                while *next_partition < partitions.len() && partitions[*next_partition].is_empty() {
                    *next_partition += 1;
                }
                if *next_partition >= partitions.len() {
                    return Ok(None);
                }
                let sources = &partitions[*next_partition];
                *next_partition += 1;
                *current = Some(PartitionMergeReader::new(sources, compression)?);
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn chunk_path_for_id(dir: &Path, id: usize) -> Option<PathBuf> {
        chunk_files_by_id(dir)
            .unwrap()
            .get(id)
            .and_then(Option::as_ref)
            .map(|(_, path)| path.clone())
    }

    fn chunk_kind_for_id(dir: &Path, id: usize) -> Option<ChunkFileKind> {
        chunk_files_by_id(dir)
            .unwrap()
            .get(id)
            .and_then(Option::as_ref)
            .map(|(kind, _)| *kind)
    }

    fn collect_keys(mut reader: SortReader) -> Vec<u64> {
        let mut out = Vec::new();
        while let Some(rec) = reader.next().unwrap() {
            out.push(rec.key);
        }
        out
    }

    fn write_presorted_test_chunk(path: &Path, keys: &[u64]) {
        let mut records: Vec<SortRecord> = keys
            .iter()
            .map(|&key| SortRecord {
                key,
                data: Box::from(key.to_le_bytes().as_slice()),
            })
            .collect();
        write_sorted_chunk(&mut records, path, ChunkCompression::None).unwrap();
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
    fn partition_from_key_uses_z6_hilbert_prefixes() {
        assert_eq!(SORT_PARTITIONS, 38_229);

        let z14_base = TILE_ID_BASES[14];
        let first = partition_from_key(make_sort_key(z14_base, 0, 0));
        let last_same_prefix = partition_from_key(make_sort_key(z14_base + 65_535, 0, 0));
        let next_prefix = partition_from_key(make_sort_key(z14_base + 65_536, 0, 0));
        assert_eq!(first, PARTITION_BASES[14]);
        assert_eq!(last_same_prefix, first);
        assert_eq!(next_prefix, first + 1);
        assert_eq!(
            partition_next_key(first),
            make_sort_key(z14_base + 65_536, 0, 0)
        );
        assert!(make_sort_key(z14_base + 65_535, u8::MAX, u8::MAX) < partition_next_key(first));

        let z13_base = TILE_ID_BASES[13];
        let z13_first = partition_from_key(make_sort_key(z13_base, 0, 0));
        let z13_next = partition_from_key(make_sort_key(z13_base + 16_384, 0, 0));
        assert_eq!(z13_first, PARTITION_BASES[13]);
        assert_eq!(z13_next, z13_first + 1);
        assert_eq!(
            partition_next_key(z13_first),
            make_sort_key(z13_base + 16_384, 0, 0)
        );
    }

    #[test]
    fn partition_ids_are_monotonic_across_zoom_boundaries() {
        let mut previous = 0usize;
        let mut first = true;
        for zoom in 0usize..15 {
            let start = TILE_ID_BASES[zoom];
            let end = TILE_ID_BASES[zoom + 1] - 1;
            let step = ((end - start) / 17).max(1);
            let mut tile_id = start;
            loop {
                let partition = partition_from_key(make_sort_key(tile_id, 0, 0));
                if !first {
                    assert!(
                        partition >= previous,
                        "partition order moved backward at tile_id {tile_id}: {partition} < {previous}",
                    );
                }
                first = false;
                previous = partition;
                if tile_id == end {
                    break;
                }
                tile_id = (tile_id + step).min(end);
            }
        }
    }

    #[test]
    fn first_cut_partition_suffix_falls_back_to_legacy_merge() {
        let dir = tempfile::tempdir().expect("create tempdir");
        write_presorted_test_chunk(&dir.path().join("chunk_0000_p227.bin"), &[10, 30]);
        write_presorted_test_chunk(&dir.path().join("chunk_0001_p000.bin"), &[20, 40]);

        let reader = SortReader::from_dir(dir.path(), Some(2), ChunkCompression::None).unwrap();
        assert!(matches!(&reader.mode, SortReaderMode::Legacy(_)));
        let keys = collect_keys(reader);
        assert_eq!(keys, vec![10, 20, 30, 40]);
    }

    #[test]
    fn uncompressed_writer_coalesces_partition_sections_into_one_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 1_000_000, ChunkCompression::None).unwrap();

        let p0 = PARTITION_BASES[14];
        let p1 = p0 + 1;
        let k0 = make_sort_key(partition_start_tile_id(p0), 0, 0);
        let k1 = make_sort_key(partition_start_tile_id(p1), 0, 0);
        let k1b = make_sort_key(partition_start_tile_id(p1) + 7, 2, 3);

        for key in [k1b, k0, k1] {
            writer
                .push(SortRecord {
                    key,
                    data: Box::from(key.to_le_bytes().as_slice()),
                })
                .unwrap();
        }
        writer.flush().unwrap();

        assert_eq!(writer.chunk_count(), 1);
        assert_eq!(chunk_kind_for_id(dir.path(), 0), Some(ChunkFileKind::Multi));

        let reader = SortReader::from_dir(dir.path(), Some(1), ChunkCompression::None).unwrap();
        assert_eq!(collect_keys(reader), vec![k0, k1, k1b]);

        let mut reader = SortReader::from_dir(dir.path(), Some(1), ChunkCompression::None).unwrap();
        let partitions = reader.take_partitions().expect("partitioned reader");
        assert_eq!(partitions.len(), 2);
        assert_eq!(partitions[0].index, p0);
        assert_eq!(partitions[1].index, p1);
        assert!(partitions[0].sources[0].section.is_some());
        assert!(partitions[1].sources[0].section.is_some());

        let mut part0 = SortPartitionReader::open(&partitions[0], ChunkCompression::None).unwrap();
        assert_eq!(part0.next().unwrap().expect("p0 record").key, k0);
        assert!(part0.next().unwrap().is_none());

        let mut part1 = SortPartitionReader::open(&partitions[1], ChunkCompression::None).unwrap();
        assert_eq!(part1.next().unwrap().expect("p1 first").key, k1);
        assert_eq!(part1.next().unwrap().expect("p1 second").key, k1b);
        assert!(part1.next().unwrap().is_none());
    }

    #[test]
    fn payload_partition_writer_coalesces_sections_into_one_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let p0 = PARTITION_BASES[14];
        let p1 = p0 + 1;
        let k0 = make_sort_key(partition_start_tile_id(p0), 0, 0);
        let k1 = make_sort_key(partition_start_tile_id(p1), 0, 0);
        let mut payload = Vec::new();
        let mut records = Vec::new();
        for key in [k1, k0] {
            let off = payload.len();
            payload.extend_from_slice(&key.to_le_bytes());
            records.push((key, off, 8));
        }

        let chunk_id = AtomicUsize::new(0);
        let paths = write_partitioned_payload_chunks(
            &mut records,
            &payload,
            dir.path(),
            &chunk_id,
            ChunkCompression::None,
        )
        .unwrap();

        assert_eq!(paths.len(), 1);
        assert_eq!(chunk_id.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(chunk_kind_for_id(dir.path(), 0), Some(ChunkFileKind::Multi));
        let reader = SortReader::from_dir(dir.path(), Some(1), ChunkCompression::None).unwrap();
        assert_eq!(collect_keys(reader), vec![k0, k1]);
    }

    #[test]
    fn single_chunk_sort() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Use a large chunk size so everything fits in one chunk.
        let mut writer = SortWriter::new(dir.path(), 1_000_000, ChunkCompression::None).unwrap();

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
        let mut writer = SortWriter::new(dir.path(), 100, ChunkCompression::None).unwrap();

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

        let writer = SortWriter::new(dir.path(), 1_000_000, ChunkCompression::None).unwrap();
        let mut reader = writer.finish().unwrap();

        assert!(reader.next().unwrap().is_none());
    }

    #[test]
    fn duplicate_keys() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Very small chunk size to force multi-chunk even with few records.
        let mut writer = SortWriter::new(dir.path(), 50, ChunkCompression::None).unwrap();

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

        let mut writer = SortWriter::new(dir.path(), 200, ChunkCompression::None).unwrap();

        // Push records with keys and payload that can be verified.
        for i in 0u64..50 {
            let key = 50 - i; // descending keys
            let mut data = Vec::new();
            data.extend_from_slice(&key.to_le_bytes());
            data.extend_from_slice(b"payload_");
            data.extend_from_slice(&i.to_le_bytes());
            writer
                .push(SortRecord {
                    key,
                    data: data.into_boxed_slice(),
                })
                .unwrap();
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
            let embedded_key = u64::from_le_bytes(record.data[..8].try_into().unwrap());
            assert_eq!(embedded_key, record.key);
        }
        assert_eq!(count, 50);
    }

    #[test]
    fn from_dir_accepts_matching_chunk_count() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 100, ChunkCompression::None).unwrap();
        for i in 0u64..200 {
            writer
                .push(SortRecord {
                    key: i,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
        }
        let n = writer.chunk_count();
        assert!(n > 1, "need multiple chunks for meaningful test");
        // finish() consumes writer but chunks remain on disk
        let _ = writer.finish().unwrap();

        // Exact match passes
        let reader = SortReader::from_dir(dir.path(), Some(n), ChunkCompression::None);
        assert!(reader.is_ok());
    }

    #[test]
    fn from_dir_rejects_chunk_count_mismatch() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 100, ChunkCompression::None).unwrap();
        for i in 0u64..200 {
            writer
                .push(SortRecord {
                    key: i,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
        }
        let n = writer.chunk_count();
        let _ = writer.finish().unwrap();

        // Wrong count is rejected
        let result = SortReader::from_dir(dir.path(), Some(n + 5), ChunkCompression::None);
        let err = result
            .err()
            .expect("expected error for mismatched chunk count");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let msg = err.to_string();
        assert!(
            msg.contains("chunk count mismatch"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn from_dir_skips_validation_when_none() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 100, ChunkCompression::None).unwrap();
        for i in 0u64..200 {
            writer
                .push(SortRecord {
                    key: i,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
        }
        let _ = writer.finish().unwrap();

        // None skips validation - always succeeds
        let reader = SortReader::from_dir(dir.path(), None, ChunkCompression::None);
        assert!(reader.is_ok());
    }

    #[test]
    fn flush_makes_chunk_count_accurate() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 10_000_000, ChunkCompression::None).unwrap();

        // Push some records (below chunk threshold)
        for i in 0u64..100 {
            writer
                .push(SortRecord {
                    key: i,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
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
        let reader = SortReader::from_dir(dir.path(), Some(count_before), ChunkCompression::None);
        assert!(reader.is_ok(), "count before finish should match disk");
    }

    #[test]
    fn flush_then_push_then_finish() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 10_000_000, ChunkCompression::None).unwrap();

        // First batch
        for i in 0u64..50 {
            writer
                .push(SortRecord {
                    key: i,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
        }
        writer.flush().unwrap();
        assert_eq!(writer.chunk_count(), 1);

        // Second batch
        for i in 50u64..100 {
            writer
                .push(SortRecord {
                    key: i,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
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
        let mut writer = SortWriter::new(dir.path(), 120, ChunkCompression::None).unwrap();
        for i in 0u64..300 {
            writer
                .push(SortRecord {
                    key: i,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
        }
        let total_chunks = writer.chunk_count();
        assert!(total_chunks >= 3, "expected >=3 chunks, got {total_chunks}");
        let _ = writer.finish().unwrap();

        // Resume from checkpoint that keeps only first 2 chunks.
        let resumed = SortWriter::resume(dir.path(), 120, 2, ChunkCompression::None).unwrap();
        assert_eq!(resumed.chunk_count(), 2);

        // Chunk id 2 should have been deleted as stale leftover.
        assert!(chunk_path_for_id(dir.path(), 2).is_none());
        // checkpoint chunks must still exist.
        assert!(chunk_path_for_id(dir.path(), 0).is_some());
        assert!(chunk_path_for_id(dir.path(), 1).is_some());
    }

    #[test]
    fn resume_fails_if_required_chunk_missing() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Create two chunks, then remove chunk_0001 to simulate corrupted checkpoint state.
        let mut writer = SortWriter::new(dir.path(), 120, ChunkCompression::None).unwrap();
        for i in 0u64..200 {
            writer
                .push(SortRecord {
                    key: i,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
        }
        assert!(writer.chunk_count() >= 2);
        let _ = writer.finish().unwrap();
        let missing_path = chunk_path_for_id(dir.path(), 1).expect("chunk 1 exists");
        std::fs::remove_file(missing_path).unwrap();

        let err = SortWriter::resume(dir.path(), 120, 2, ChunkCompression::None)
            .err()
            .expect("resume should fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("missing chunk file"));
    }

    #[test]
    fn resume_allows_empty_checkpoint() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let writer = SortWriter::resume(dir.path(), 1024, 0, ChunkCompression::None).unwrap();
        assert_eq!(writer.chunk_count(), 0);
        let reader = writer.finish().unwrap();
        let keys = collect_keys(reader);
        assert!(keys.is_empty());
    }

    #[test]
    fn resume_start_chunk_zero_deletes_stale_chunks() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Seed stale chunks from a previous interrupted run.
        let mut seeded = SortWriter::new(dir.path(), 120, ChunkCompression::None).unwrap();
        for i in 0u64..200 {
            seeded
                .push(SortRecord {
                    key: i,
                    data: Box::from(i.to_le_bytes().as_slice()),
                })
                .unwrap();
        }
        assert!(seeded.chunk_count() >= 2, "expected stale chunks to exist");
        let _ = seeded.finish().unwrap();
        assert!(chunk_path_for_id(dir.path(), 0).is_some());

        // Empty-checkpoint resume should prune all stale chunks and start clean.
        let resumed = SortWriter::resume(dir.path(), 120, 0, ChunkCompression::None).unwrap();
        assert_eq!(resumed.chunk_count(), 0);
        assert!(chunk_path_for_id(dir.path(), 0).is_none());
        assert!(chunk_path_for_id(dir.path(), 1).is_none());

        // New writes should restart naming from chunk_0000.bin.
        let mut resumed = resumed;
        resumed
            .push(SortRecord {
                key: 7,
                data: Box::from(7u64.to_le_bytes().as_slice()),
            })
            .unwrap();
        resumed.flush().unwrap();
        assert!(chunk_path_for_id(dir.path(), 0).is_some());
    }

    #[test]
    fn adopt_chunk_files_updates_count_and_merges_records() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // Main writer with one in-memory batch.
        let mut writer = SortWriter::new(dir.path(), 10_000_000, ChunkCompression::None).unwrap();
        writer
            .push(SortRecord {
                key: 40,
                data: Box::from(40u64.to_le_bytes().as_slice()),
            })
            .unwrap();
        writer
            .push(SortRecord {
                key: 20,
                data: Box::from(20u64.to_le_bytes().as_slice()),
            })
            .unwrap();

        // External chunk A.
        let ext_a = dir.path().join("external_a.bin");
        let mut recs_a = vec![
            SortRecord {
                key: 10,
                data: Box::from(10u64.to_le_bytes().as_slice()),
            },
            SortRecord {
                key: 30,
                data: Box::from(30u64.to_le_bytes().as_slice()),
            },
        ];
        write_sorted_chunk(&mut recs_a, &ext_a, ChunkCompression::None).unwrap();

        // External chunk B.
        let ext_b = dir.path().join("external_b.bin");
        let mut recs_b = vec![
            SortRecord {
                key: 5,
                data: Box::from(5u64.to_le_bytes().as_slice()),
            },
            SortRecord {
                key: 50,
                data: Box::from(50u64.to_le_bytes().as_slice()),
            },
        ];
        write_sorted_chunk(&mut recs_b, &ext_b, ChunkCompression::None).unwrap();

        writer.adopt_chunk_files(vec![ext_a, ext_b]);
        assert_eq!(
            writer.chunk_count(),
            2,
            "adopted chunks should count immediately"
        );

        // finish flushes writer buffer as chunk_0002.bin
        let reader = writer.finish().unwrap();
        let keys = collect_keys(reader);
        assert_eq!(keys, vec![5, 10, 20, 30, 40, 50]);
    }

    #[test]
    fn shared_chunk_counter_avoids_collision() {
        // Models the way phase: a producer thread allocates chunk numbers from a
        // shared counter and hands files to the drain writer via adopt, while the
        // writer's own flushes must draw from the SAME counter so no two chunks
        // claim the same chunk_NNNN.bin. Regression guard for the drain-vs-task
        // chunk-id collision.
        let dir = tempfile::tempdir().expect("create tempdir");
        // chunk_size 1 forces a flush on every push.
        let mut writer = SortWriter::new(dir.path(), 1, ChunkCompression::None).unwrap();
        let counter = Arc::new(AtomicUsize::new(writer.chunk_count()));
        writer.attach_chunk_counter(Arc::clone(&counter));

        // "Task" allocates chunk 0 and writes it, then hands it over.
        let task_no = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(task_no, 0);
        let task_path = dir.path().join(format!("chunk_{task_no:04}.bin"));
        let mut task_recs = vec![SortRecord {
            key: 10,
            data: Box::from(10u64.to_le_bytes().as_slice()),
        }];
        write_sorted_chunk(&mut task_recs, &task_path, ChunkCompression::None).unwrap();
        writer.adopt_chunk_files(vec![task_path]);

        // The writer's own flushes must skip 0 (taken by the task) and use 1, 2.
        writer
            .push(SortRecord {
                key: 20,
                data: Box::from(20u64.to_le_bytes().as_slice()),
            })
            .unwrap();
        writer
            .push(SortRecord {
                key: 30,
                data: Box::from(30u64.to_le_bytes().as_slice()),
            })
            .unwrap();

        writer.detach_chunk_counter();
        assert_eq!(writer.chunk_count(), 3, "3 distinct chunks allocated");
        for i in 0..3 {
            assert!(
                chunk_path_for_id(dir.path(), i).is_some(),
                "chunk id {i} missing - a flush collided and overwrote it"
            );
        }

        let reader = writer.finish().unwrap();
        let keys = collect_keys(reader);
        assert_eq!(keys, vec![10, 20, 30], "no records lost to a collision");
    }

    #[test]
    fn adopt_chunk_files_missing_or_corrupt_surfaces_error() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 10_000_000, ChunkCompression::None).unwrap();
        writer
            .push(SortRecord {
                key: 1,
                data: Box::from(1u64.to_le_bytes().as_slice()),
            })
            .unwrap();

        let missing = dir.path().join("does_not_exist.bin");
        writer.adopt_chunk_files(vec![missing]);
        let missing_err = writer
            .finish()
            .err()
            .expect("missing adopted chunk should fail");
        assert_eq!(missing_err.kind(), io::ErrorKind::NotFound);

        let mut writer2 = SortWriter::new(dir.path(), 10_000_000, ChunkCompression::None).unwrap();
        writer2
            .push(SortRecord {
                key: 2,
                data: Box::from(2u64.to_le_bytes().as_slice()),
            })
            .unwrap();

        // Corrupt chunk header (too short for u32 record count).
        let corrupt = dir.path().join("corrupt.bin");
        std::fs::write(&corrupt, [0xAA, 0xBB]).unwrap();
        writer2.adopt_chunk_files(vec![corrupt]);
        let corrupt_err = writer2
            .finish()
            .err()
            .expect("corrupt adopted chunk should fail");
        assert_eq!(corrupt_err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
