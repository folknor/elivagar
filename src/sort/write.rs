use super::*;
use std::sync::atomic::AtomicUsize;

/// Cloneable handle for writing chunks into a sort writer's chunk store.
///
/// The store creates this sink at its current next chunk id. Producers can
/// share clones without constructing or initializing an allocator themselves.
#[derive(Clone)]
pub struct ChunkSink {
    tmp_dir: PathBuf,
    next_chunk_id: Arc<AtomicUsize>,
    compression: ChunkCompression,
}

impl ChunkSink {
    fn new(tmp_dir: PathBuf, next_chunk_id: usize, compression: ChunkCompression) -> Self {
        Self {
            tmp_dir,
            next_chunk_id: Arc::new(AtomicUsize::new(next_chunk_id)),
            compression,
        }
    }

    fn allocate_chunk_id(&self) -> usize {
        self.next_chunk_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn chunk_count(&self) -> usize {
        self.next_chunk_id
            .load(std::sync::atomic::Ordering::Relaxed)
    }
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
    compression: ChunkCompression,
    tally: RecordTally,
    chunk_sink: ChunkSink,
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
        let tmp_dir = tmp_dir.to_path_buf();
        Ok(SortWriter {
            tmp_dir: tmp_dir.clone(),
            buffer: Vec::new(),
            buffer_bytes: 0,
            chunk_size_bytes,
            chunk_paths: Vec::new(),
            compression,
            tally: RecordTally::new(),
            chunk_sink: ChunkSink::new(tmp_dir, 0, compression),
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
                    let path = multi_chunk_path(tmp_dir, i);
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
            compression,
            tally: RecordTally::new(),
            chunk_sink: ChunkSink::new(tmp_dir.to_path_buf(), start_chunk, compression),
        })
    }

    /// Number of chunk ids allocated so far, including retained ids from resume.
    pub fn chunk_count(&self) -> usize {
        self.chunk_sink.chunk_count()
    }

    /// Total sort records pushed (across all chunks + current buffer).
    pub fn total_records(&self) -> u64 {
        self.tally.total_records
    }

    /// Total payload bytes pushed (sum of record.data.len()).
    pub fn total_record_bytes(&self) -> u64 {
        self.tally.total_record_bytes
    }

    /// Per-layer record counts (indexed by layer id, 0..26).
    pub fn layer_records(&self) -> &[u64; crate::shortbread::Layer::count()] {
        &self.tally.layer_records
    }

    /// Per-layer payload bytes (indexed by layer id, 0..26).
    pub fn layer_bytes(&self) -> &[u64; crate::shortbread::Layer::count()] {
        &self.tally.layer_bytes
    }

    /// Per-layer-per-zoom record counts, indexed by layer then zoom.
    pub(crate) fn layer_zoom_records(&self) -> &PerLayerZoom<u64> {
        &self.tally.layer_zoom_records
    }

    /// Per-layer-per-zoom payload bytes, indexed by layer then zoom.
    pub(crate) fn layer_zoom_bytes(&self) -> &PerLayerZoom<u64> {
        &self.tally.layer_zoom_bytes
    }

    /// Add a record to the buffer. If the buffer exceeds `chunk_size_bytes`,
    /// the current buffer is sorted and flushed to a chunk file on disk.
    pub fn push(&mut self, record: SortRecord) -> io::Result<()> {
        let data_len = record.data.len();
        self.buffer_bytes += data_len + std::mem::size_of::<SortRecord>();
        self.tally.record(record.key, data_len);
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

    /// Clone the store-owned sink for concurrent or direct chunk producers.
    pub fn chunk_sink(&self) -> ChunkSink {
        self.chunk_sink.clone()
    }

    pub(crate) fn merge_tally(&mut self, tally: &RecordTally) {
        self.tally.merge(tally);
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

    /// Adopt chunk files written through this writer's `ChunkSink`.
    /// The sink already owns their ids, so adopting only adds them to the
    /// manifest and does not separately reconstruct the chunk count.
    pub fn adopt_chunk_files(&mut self, paths: Vec<PathBuf>) {
        self.chunk_paths.extend(paths);
    }

    /// Sort the in-memory buffer by the total record order and write it to disk.
    fn flush_chunk(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        sort_records_total(&mut self.buffer);
        let chunk_no = self.chunk_sink.allocate_chunk_id();
        let path =
            write_partitioned_sort_chunk(&self.buffer, &self.tmp_dir, chunk_no, self.compression)?;

        self.chunk_paths.push(path);
        self.buffer.clear();
        self.buffer_bytes = 0;
        Ok(())
    }
}

/// Write records as a sorted chunk file in the standard format.
///
/// Records are sorted in-place by key and payload, then written as:
/// ```text
/// u32 record_count
/// For each record:
///   u64 key
///   u32 data_len
///   [u8; data_len] data
/// ```
///
/// Writes to an explicit path. Store-managed producers should use `ChunkSink`
/// so chunk ids come from the owning sort writer.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
pub fn write_sorted_chunk(
    records: &mut [SortRecord],
    path: &Path,
    compression: ChunkCompression,
) -> io::Result<()> {
    sort_records_total(records);
    write_chunk_records_presorted(records, path, compression)
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
    sort_payload_records_total(records, payload);
    write_payload_chunk_records_presorted(records, payload, path, compression)
}

/// Write arena-backed records as partition-pure sorted chunk files.
///
/// The chunk store provides a shared sink to direct producers. Returned paths
/// are suitable for `SortWriter::adopt_chunk_files` on that store.
#[hotpath::measure]
pub fn write_partitioned_payload_chunks(
    records: &mut [PayloadRecord],
    payload: &[u8],
    sink: &ChunkSink,
) -> io::Result<Vec<PathBuf>> {
    if records.is_empty() {
        return Ok(Vec::new());
    }
    sort_payload_records_total(records, payload);
    let id = sink.allocate_chunk_id();
    let path =
        write_partitioned_payload_chunk(records, payload, &sink.tmp_dir, id, sink.compression)?;
    Ok(vec![path])
}

// ---------------------------------------------------------------------------
// SpillCoalescer - merges many small producer flushes into big sorted chunks
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CoalesceBuf {
    records: Vec<PayloadRecord>,
    payload: Vec<u8>,
}

impl CoalesceBuf {
    fn bytes(&self) -> usize {
        self.payload.len() + self.records.len() * std::mem::size_of::<PayloadRecord>()
    }
}

/// Shared accumulator between parallel record producers (way tasks, relation
/// workers, ocean folds) and the chunk directory. Producers bulk-append their
/// small sinks (a memcpy under a mutex); the coalescer writes one big sorted
/// multi-partition chunk per `budget` bytes, outside the lock.
///
/// Why it exists: producers flushing their own sinks straight to
/// `write_partitioned_payload_chunks` fragments the scratch. Measured on the
/// NA locations re-baseline (`6a13f306`): 2754 chunk files at 4-64 MB, each
/// splitting into up to 15442 partition sections of ~4 KB, drove 95.8 GB of
/// assemble-phase disk reads against 68.4 GB of merge bytes (readahead
/// over-fetch on tiny random reads) plus 210K major faults, regressing
/// assemble by ~35s versus pre-fragmentation baselines. Coalesced ~1 GB
/// chunks keep per-partition sections in the tens-of-KB range and the merge
/// fan-in near the pre-campaign 28-70.
pub struct SpillCoalescer {
    buf: std::sync::Mutex<CoalesceBuf>,
    budget: usize,
    sink: ChunkSink,
    paths: std::sync::Mutex<Vec<PathBuf>>,
}

impl SpillCoalescer {
    pub fn new(sink: ChunkSink, budget: usize) -> Self {
        Self {
            buf: std::sync::Mutex::new(CoalesceBuf::default()),
            budget,
            sink,
            paths: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Bulk-append one producer sink. Record offsets are rebased onto the
    /// shared payload. If the append trips the budget, the full buffer is
    /// swapped out and written as a sorted chunk OUTSIDE the lock, so other
    /// producers only ever wait for memcpys, never disk writes. Two producers
    /// tripping the budget concurrently both write (each a full-ish buffer);
    /// the shared store-owned sink keeps the files distinct.
    pub fn append(&self, records: &[PayloadRecord], payload: &[u8]) {
        if records.is_empty() {
            return;
        }
        let full = {
            let mut buf = self.buf.lock().expect("spill coalescer lock");
            let base = buf.payload.len();
            buf.payload.extend_from_slice(payload);
            buf.records.extend(
                records
                    .iter()
                    .map(|&(key, off, len)| (key, off + base, len)),
            );
            if buf.bytes() >= self.budget {
                Some(std::mem::take(&mut *buf))
            } else {
                None
            }
        };
        if let Some(full) = full {
            self.write(full);
        }
    }

    fn write(&self, mut buf: CoalesceBuf) {
        if buf.records.is_empty() {
            return;
        }
        // Panic: called from rayon workers and phase tails - disk I/O failure
        // is unrecoverable here, same policy as the producer sinks had.
        let paths = write_partitioned_payload_chunks(&mut buf.records, &buf.payload, &self.sink)
            .expect("coalesced chunk write failed");
        self.paths
            .lock()
            .expect("spill coalescer paths lock")
            .extend(paths);
    }

    /// Write any residual buffered records and return every chunk path this
    /// coalescer produced, ready for `SortWriter::adopt_chunk_files`. The
    /// coalescer is drained but reusable (a later phase may keep appending;
    /// call `finish` again for the new paths).
    pub fn finish(&self) -> Vec<PathBuf> {
        let residual = std::mem::take(&mut *self.buf.lock().expect("spill coalescer lock"));
        self.write(residual);
        std::mem::take(&mut *self.paths.lock().expect("spill coalescer paths lock"))
    }
}
