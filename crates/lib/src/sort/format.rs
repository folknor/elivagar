use super::*;

pub(super) const MULTI_CHUNK_MAGIC: &[u8; 8] = b"ELVGSRT1";
pub(super) const MULTI_CHUNK_MAGIC_LZ4: &[u8; 8] = b"ELVGSRL1";
pub(super) const MULTI_CHUNK_MAGIC_SNAPPY: &[u8; 8] = b"ELVGSRS1";

/// Multi-partition chunk files carry a per-compression magic so a
/// `--skip-to` resume with a different `--compress-sort-chunks` setting
/// fails loudly at open instead of feeding the merge garbage.
pub(super) fn multi_chunk_magic(compression: ChunkCompression) -> &'static [u8; 8] {
    match compression {
        ChunkCompression::None => MULTI_CHUNK_MAGIC,
        ChunkCompression::Lz4 => MULTI_CHUNK_MAGIC_LZ4,
        ChunkCompression::Snappy => MULTI_CHUNK_MAGIC_SNAPPY,
    }
}

// ---------------------------------------------------------------------------
// Chunk compression selection
// ---------------------------------------------------------------------------

/// Compression algorithm for sort chunk files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkCompression {
    /// No compression (default). Fastest writes, largest files.
    #[default]
    None,
    /// LZ4 frame compression (lz4_flex). Good ratio, pure Rust.
    Lz4,
    /// Snappy frame compression (snap crate). Lower per-call overhead.
    Snappy,
}

/// Maximum additional serialization memory per active compressed chunk write.
pub(super) const COMPRESSED_CHUNK_BUFFER_SIZE: usize = 1 << 20;

/// Coalesces the small record fields into bounded writes before they reach a
/// frame encoder. Large payloads pass through directly after the pending bytes
/// are flushed, so memory does not grow with the chunk size.
pub(super) struct ChunkSerializationBuffer<'a> {
    writer: &'a mut dyn Write,
    buffer: Vec<u8>,
}

impl<'a> ChunkSerializationBuffer<'a> {
    fn with_buffer(writer: &'a mut dyn Write, mut buffer: Vec<u8>) -> Self {
        buffer.clear();
        Self { writer, buffer }
    }

    fn flush_buffer(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            self.writer.write_all(&self.buffer)?;
            self.buffer.clear();
        }
        Ok(())
    }

    fn into_buffer(mut self) -> io::Result<Vec<u8>> {
        self.flush_buffer()?;
        Ok(self.buffer)
    }
}

impl Write for ChunkSerializationBuffer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if bytes.len() >= COMPRESSED_CHUNK_BUFFER_SIZE {
            self.flush_buffer()?;
            self.writer.write_all(bytes)?;
            return Ok(bytes.len());
        }
        if self.buffer.len() + bytes.len() > COMPRESSED_CHUNK_BUFFER_SIZE {
            self.flush_buffer()?;
        }
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buffer()?;
        self.writer.flush()
    }
}

pub(super) fn write_compressed_frame<W: Write>(
    writer: &mut W,
    compression: ChunkCompression,
    scratch: &mut Vec<u8>,
    serialize: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    match compression {
        ChunkCompression::None => unreachable!(),
        ChunkCompression::Lz4 => {
            let mut encoder = FrameEncoder::new(writer);
            let mut buffered =
                ChunkSerializationBuffer::with_buffer(&mut encoder, std::mem::take(scratch));
            serialize(&mut buffered)?;
            *scratch = buffered.into_buffer()?;
            encoder.finish().map_err(io::Error::other)?;
        }
        ChunkCompression::Snappy => {
            let mut encoder = snap::write::FrameEncoder::new(writer);
            let mut buffered =
                ChunkSerializationBuffer::with_buffer(&mut encoder, std::mem::take(scratch));
            serialize(&mut buffered)?;
            *scratch = buffered.into_buffer()?;
            encoder.flush()?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Chunk I/O abstraction - compressed reads
// ---------------------------------------------------------------------------

pub(super) enum ChunkRead {
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

pub(super) fn chunk_path(tmp_dir: &Path, chunk_no: usize, partition: usize) -> PathBuf {
    tmp_dir.join(format!(
        "chunk_{chunk_no:04}_z{PARTITION_SPLIT_Z}p{partition:05}.bin"
    ))
}

pub(super) fn multi_chunk_path(tmp_dir: &Path, chunk_no: usize) -> PathBuf {
    tmp_dir.join(format!("chunk_{chunk_no:04}_z{PARTITION_SPLIT_Z}m.bin"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChunkFileKind {
    Partition(usize),
    Multi,
}

pub(super) fn parse_chunk_filename(
    path: &Path,
) -> Result<Option<(usize, ChunkFileKind)>, ChunkStoreError> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    if !name.starts_with("chunk_") || !name.ends_with(".bin") {
        return Ok(None);
    }
    let invalid = || {
        ChunkStoreError::new(
            ChunkStoreErrorKind::Stale,
            format!(
                "unsupported sort chunk name {name}; regenerate chunks with split zoom {PARTITION_SPLIT_Z}"
            ),
        )
    };
    let body = &name[6..name.len() - 4];
    let (id, suffix) = body.split_once("_z").ok_or_else(invalid)?;
    let chunk_no = id.parse().map_err(|_| invalid())?;
    if let Some(zoom) = suffix.strip_suffix('m') {
        if zoom.parse::<u8>().map_err(|_| invalid())? != PARTITION_SPLIT_Z {
            return Err(invalid());
        }
        return Ok(Some((chunk_no, ChunkFileKind::Multi)));
    }
    let (zoom, partition) = suffix.split_once('p').ok_or_else(invalid)?;
    let partition: usize = partition.parse().map_err(|_| invalid())?;
    if zoom.parse::<u8>().map_err(|_| invalid())? != PARTITION_SPLIT_Z
        || partition >= SORT_PARTITIONS
    {
        return Err(invalid());
    }
    Ok(Some((chunk_no, ChunkFileKind::Partition(partition))))
}

pub(super) type ChunkScanEntry = (usize, ChunkFileKind, PathBuf);
pub(super) type ChunkById = Vec<Option<(ChunkFileKind, PathBuf)>>;

pub(super) fn scan_chunk_files(tmp_dir: &Path) -> Result<Vec<ChunkScanEntry>, ChunkStoreError> {
    let mut out = Vec::new();
    match fs::read_dir(tmp_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if let Some((chunk_no, kind)) = parse_chunk_filename(&path)? {
                    out.push((chunk_no, kind, path));
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    out.sort_unstable_by_key(|(chunk_no, _, _)| *chunk_no);
    Ok(out)
}

pub(super) fn chunk_files_by_id(tmp_dir: &Path) -> Result<ChunkById, ChunkStoreError> {
    let scanned = scan_chunk_files(tmp_dir)?;
    let max_id = scanned
        .iter()
        .map(|(chunk_no, _, _)| *chunk_no)
        .max()
        .map_or(0, |id| id + 1);
    let mut by_id = vec![None; max_id];
    for (chunk_no, kind, path) in scanned {
        if by_id[chunk_no].is_some() {
            return Err(ChunkStoreError::new(
                ChunkStoreErrorKind::Stale,
                format!("duplicate chunk id {chunk_no} in {}", tmp_dir.display()),
            ));
        }
        by_id[chunk_no] = Some((kind, path));
    }
    Ok(by_id)
}

#[allow(clippy::cast_possible_truncation)]
pub(super) fn write_chunk_records_presorted(
    records: &[SortRecord],
    path: &Path,
    compression: ChunkCompression,
) -> io::Result<()> {
    write_records(records, None, path, compression)
}

#[derive(Clone, Copy)]
pub(super) struct PartitionRange {
    partition: usize,
    start: usize,
    end: usize,
}

pub(super) fn partition_ranges_by_key(
    len: usize,
    mut key_at: impl FnMut(usize) -> SortKey,
) -> Vec<PartitionRange> {
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < len {
        let partition = key_at(start).partition();
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

/// The staging owns payloads either per record or in a shared arena. Both
/// sources serialize through the same layout and compression path.
pub(super) trait RecordSource {
    fn len(&self) -> usize;
    fn record(&self, index: usize) -> (SortKey, &[u8]);
}

impl RecordSource for [SortRecord] {
    fn len(&self) -> usize {
        self.len()
    }
    fn record(&self, index: usize) -> (SortKey, &[u8]) {
        (self[index].key, &self[index].data)
    }
}

pub(super) struct PayloadSource<'a>(&'a [PayloadRef], &'a [u8]);
impl RecordSource for PayloadSource<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn record(&self, index: usize) -> (SortKey, &[u8]) {
        let record = self.0[index];
        (record.key, record.bytes(self.1))
    }
}

#[allow(clippy::cast_possible_truncation)]
pub(super) fn write_records<S: RecordSource + ?Sized>(
    source: &S,
    ranges: Option<&[PartitionRange]>,
    path: &Path,
    compression: ChunkCompression,
) -> io::Result<()> {
    let _wait = wait_span(&WAIT.sort_chunk_write);
    let serialize = |out: &mut dyn Write, start: usize, end: usize| -> io::Result<()> {
        for index in start..end {
            let (key, data) = source.record(index);
            out.write_all(&key.raw().to_le_bytes())?;
            out.write_all(&(data.len() as u32).to_le_bytes())?;
            out.write_all(data)?;
        }
        Ok(())
    };
    let file = File::create(path)?;
    let mut writer = BufWriter::with_capacity(1 << 20, file);
    let mut scratch = Vec::with_capacity(if compression == ChunkCompression::None {
        0
    } else {
        COMPRESSED_CHUNK_BUFFER_SIZE
    });
    if let Some(ranges) = ranges {
        writer.write_all(multi_chunk_magic(compression))?;
        writer.write_all(&(ranges.len() as u32).to_le_bytes())?;
        let mut sections = Vec::with_capacity(ranges.len());
        if compression == ChunkCompression::None {
            // Plain offsets are known before writing. Keep this path sequential:
            // stream_position would flush the file buffer at every section.
            let mut offset = 12 + ranges.len() as u64 * 16;
            for range in ranges {
                sections.push((range.partition, range.end - range.start, offset));
                for index in range.start..range.end {
                    offset += 12 + source.record(index).1.len() as u64;
                }
            }
        } else {
            writer.write_all(&vec![0u8; ranges.len() * 16])?;
            for range in ranges {
                let offset = writer.stream_position()?;
                write_compressed_frame(&mut writer, compression, &mut scratch, |out| {
                    serialize(out, range.start, range.end)
                })?;
                sections.push((range.partition, range.end - range.start, offset));
            }
            // Compressed sizes are known only after encoding each independent
            // frame. Patch the same section table without staging chunk bytes.
            writer.seek(SeekFrom::Start(12))?;
        }
        for (partition, count, offset) in sections {
            writer.write_all(&(partition as u32).to_le_bytes())?;
            writer.write_all(&(count as u32).to_le_bytes())?;
            writer.write_all(&offset.to_le_bytes())?;
        }
        if compression == ChunkCompression::None {
            for range in ranges {
                serialize(&mut writer, range.start, range.end)?;
            }
        }
    } else if compression == ChunkCompression::None {
        writer.write_all(&(source.len() as u32).to_le_bytes())?;
        serialize(&mut writer, 0, source.len())?;
    } else {
        write_compressed_frame(&mut writer, compression, &mut scratch, |out| {
            out.write_all(&(source.len() as u32).to_le_bytes())?;
            serialize(out, 0, source.len())
        })?;
    }
    writer.flush()
}

pub(super) fn write_partitioned_records<S: RecordSource + ?Sized>(
    source: &S,
    tmp_dir: &Path,
    chunk_no: usize,
    compression: ChunkCompression,
) -> io::Result<PathBuf> {
    let ranges = partition_ranges_by_key(source.len(), |index| source.record(index).0);
    let path = if ranges.len() == 1 {
        chunk_path(tmp_dir, chunk_no, ranges[0].partition)
    } else {
        multi_chunk_path(tmp_dir, chunk_no)
    };
    write_records(
        source,
        if ranges.len() == 1 {
            None
        } else {
            Some(&ranges)
        },
        &path,
        compression,
    )?;
    Ok(path)
}

pub(super) fn write_partitioned_sort_chunk(
    records: &[SortRecord],
    tmp_dir: &Path,
    chunk_no: usize,
    compression: ChunkCompression,
) -> io::Result<PathBuf> {
    write_partitioned_records(records, tmp_dir, chunk_no, compression)
}

pub(super) fn write_partitioned_payload_chunk(
    records: &[PayloadRef],
    payload: &[u8],
    tmp_dir: &Path,
    chunk_no: usize,
    compression: ChunkCompression,
) -> io::Result<PathBuf> {
    write_partitioned_records(
        &PayloadSource(records, payload),
        tmp_dir,
        chunk_no,
        compression,
    )
}

#[allow(clippy::cast_possible_truncation)]
pub(super) fn write_payload_chunk_records_presorted(
    records: &[PayloadRef],
    payload: &[u8],
    path: &Path,
    compression: ChunkCompression,
) -> io::Result<()> {
    write_records(&PayloadSource(records, payload), None, path, compression)
}

// ---------------------------------------------------------------------------
// ChunkReader - reads records sequentially from a single chunk file
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub(super) struct MultiChunkSection {
    pub(super) partition: usize,
    pub(super) offset: u64,
    pub(super) count: u32,
    /// On-disk bytes from this section's offset to the next section (or file
    /// end). Exact record bytes for uncompressed chunks; compressed frame
    /// bytes otherwise - either way a size signal for hot-partition
    /// splitting, never an addressing fact.
    pub(super) byte_extent: u64,
}

#[derive(Clone, Debug)]
pub(super) struct SortPartitionSource {
    pub(super) path: PathBuf,
    pub(super) section: Option<MultiChunkSection>,
    /// Size signal mirroring `MultiChunkSection::byte_extent`; for whole-file
    /// sources the file's record bytes (uncompressed) or compressed bytes.
    pub(super) byte_extent: u64,
}

impl SortPartitionSource {
    pub(super) fn whole(path: PathBuf, byte_extent: u64) -> Self {
        Self {
            path,
            section: None,
            byte_extent,
        }
    }

    pub(super) fn section(path: PathBuf, section: MultiChunkSection) -> Self {
        Self {
            path,
            byte_extent: section.byte_extent,
            section: Some(section),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SourcePosition {
    pub(crate) records: u64,
    pub(crate) bytes: u64,
}

/// A lightweight handle for scanning one sort source and seeking piece ranges.
#[derive(Clone)]
pub(crate) struct SourceSeek {
    source: SortPartitionSource,
    compression: ChunkCompression,
    partition_index: usize,
}

impl SourceSeek {
    pub(super) fn new(
        source: SortPartitionSource,
        compression: ChunkCompression,
        partition_index: usize,
    ) -> Self {
        Self {
            source,
            compression,
            partition_index,
        }
    }

    pub(crate) fn cursor(&self) -> Result<SourceCursor, ChunkStoreError> {
        Ok(SourceCursor {
            reader: ChunkReader::open_source(
                &self.source,
                self.compression,
                partition_tile_range(self.partition_index),
            )?,
            position: SourcePosition::default(),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.source.path
    }

    pub(crate) fn slice(
        &self,
        start: SourcePosition,
        end: SourcePosition,
    ) -> io::Result<Option<SortSourceSlice>> {
        let records = end
            .records
            .checked_sub(start.records)
            .ok_or_else(|| io::Error::other("piece source record range is reversed"))?;
        if records == 0 {
            return Ok(None);
        }
        let take_records = u32::try_from(records)
            .map_err(|_| io::Error::other("piece record count exceeds u32"))?;
        let source = if self.compression == ChunkCompression::None {
            let data_base = self
                .source
                .section
                .as_ref()
                .map_or(4, |section| section.offset);
            let offset = data_base
                .checked_add(start.bytes)
                .ok_or_else(|| io::Error::other("piece source byte offset overflows u64"))?;
            let byte_extent = end
                .bytes
                .checked_sub(start.bytes)
                .ok_or_else(|| io::Error::other("piece source byte range is reversed"))?;
            SortPartitionSource {
                path: self.source.path.clone(),
                byte_extent,
                section: Some(MultiChunkSection {
                    partition: self.partition_index,
                    offset,
                    count: take_records,
                    byte_extent,
                }),
            }
        } else {
            self.source.clone()
        };
        Ok(Some(SortSourceSlice {
            source,
            skip_records: if self.compression == ChunkCompression::None {
                0
            } else {
                start.records
            },
            take_records,
            tile_range: partition_tile_range(self.partition_index),
        }))
    }
}

pub(crate) struct SourceCursor {
    reader: ChunkReader,
    position: SourcePosition,
}

impl SourceCursor {
    pub(crate) fn position(&self) -> SourcePosition {
        self.position
    }

    pub(crate) fn next_header_discard_payload(
        &mut self,
    ) -> Result<Option<(SortKey, u32)>, ChunkStoreError> {
        let Some((key, data_len)) = self.reader.next_header_discard_payload()? else {
            return Ok(None);
        };
        self.position.records += 1;
        self.position.bytes += 12 + u64::from(data_len);
        Ok(Some((key, data_len)))
    }
}

pub(crate) struct SortSourceSlice {
    pub(super) source: SortPartitionSource,
    pub(super) skip_records: u64,
    pub(super) take_records: u32,
    pub(super) tile_range: (u64, u64),
}

/// One section-table entry against its predecessor and the file: the
/// writer's layout is partitions strictly increasing, sections back to back
/// from the header end, and no zero-record section.
fn check_section_entry(
    prev: Option<&MultiChunkSection>,
    partition: usize,
    count: u32,
    offset: u64,
    header_end: u64,
    file_len: u64,
) -> Result<(), String> {
    if count == 0 {
        return Err(format!("partition {partition} has zero records"));
    }
    match prev {
        Some(prev) if partition <= prev.partition => {
            return Err(format!(
                "partition {partition} follows partition {}",
                prev.partition
            ));
        }
        Some(prev) if offset <= prev.offset => {
            return Err(format!(
                "offset {offset} does not follow offset {}",
                prev.offset
            ));
        }
        None if offset != header_end => {
            return Err(format!(
                "first section at {offset}, header ends at {header_end}"
            ));
        }
        _ => {}
    }
    if offset >= file_len {
        return Err(format!("offset {offset} is past the {file_len}-byte file"));
    }
    Ok(())
}

pub(super) fn read_multi_chunk_sections(
    path: &Path,
    compression: ChunkCompression,
) -> Result<Vec<MultiChunkSection>, ChunkStoreError> {
    let mut file = File::open(path).map_err(ChunkStoreError::open)?;
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic).map_err(ChunkStoreError::read)?;
    if &magic != multi_chunk_magic(compression) {
        let written_with = [
            ChunkCompression::None,
            ChunkCompression::Lz4,
            ChunkCompression::Snappy,
        ]
        .into_iter()
        .find(|&c| &magic == multi_chunk_magic(c));
        return Err(ChunkStoreError::new(
            if written_with.is_some() {
                ChunkStoreErrorKind::CompressionMismatch
            } else {
                ChunkStoreErrorKind::Corrupt
            },
            match written_with {
                Some(actual) => format!(
                    "multi-partition chunk {} was written with {actual:?} compression \
                     but this run expects {compression:?} - the --compress-sort-chunks \
                     setting must match the run that wrote the chunks",
                    path.display()
                ),
                None => format!("invalid multi-partition chunk magic in {}", path.display()),
            },
        ));
    }

    // This in-file table is the only section index. There is deliberately no
    // persisted chunk-store manifest duplicating it: reopening every chunk
    // header at SortReader::new measured 1.6 s of a ~600 s planet build (290
    // chunks), assembly opens each section by offset either way, and a key's
    // valid tile range already follows from the section's partition id. So
    // the table is validated here against what the writer produces rather
    // than trusted: partitions strictly increase, the first section starts at
    // the header end, offsets strictly increase within the file, and every
    // section holds at least one record. A multi file with zero sections
    // (empty input) is valid.
    let invalid = |what: String| {
        ChunkStoreError::new(
            ChunkStoreErrorKind::Corrupt,
            format!("corrupt section table in {}: {what}", path.display()),
        )
    };
    let file_len = file.metadata()?.len();
    let mut buf4 = [0u8; 4];
    file.read_exact(&mut buf4).map_err(ChunkStoreError::read)?;
    let section_count = u32::from_le_bytes(buf4);
    if section_count as usize > SORT_PARTITIONS {
        return Err(invalid(format!(
            "{section_count} sections exceed {SORT_PARTITIONS} partitions"
        )));
    }
    let header_end = 12 + 16 * u64::from(section_count);
    if header_end > file_len {
        return Err(invalid(format!(
            "{section_count}-section table overruns the {file_len}-byte file"
        )));
    }
    let section_count = section_count as usize;
    let mut sections: Vec<MultiChunkSection> = Vec::with_capacity(section_count);
    for _ in 0..section_count {
        file.read_exact(&mut buf4).map_err(ChunkStoreError::read)?;
        let partition = usize::try_from(u32::from_le_bytes(buf4)).map_err(|_| {
            ChunkStoreError::new(
                ChunkStoreErrorKind::Corrupt,
                format!("partition id does not fit usize in {}", path.display()),
            )
        })?;
        if partition >= SORT_PARTITIONS {
            return Err(ChunkStoreError::new(
                ChunkStoreErrorKind::Corrupt,
                format!("invalid partition {partition} in {}", path.display()),
            ));
        }

        file.read_exact(&mut buf4).map_err(ChunkStoreError::read)?;
        let count = u32::from_le_bytes(buf4);

        let mut buf8 = [0u8; 8];
        file.read_exact(&mut buf8).map_err(ChunkStoreError::read)?;
        let offset = u64::from_le_bytes(buf8);

        check_section_entry(
            sections.last(),
            partition,
            count,
            offset,
            header_end,
            file_len,
        )
        .map_err(invalid)?;

        sections.push(MultiChunkSection {
            partition,
            offset,
            count,
            byte_extent: 0,
        });
    }
    // Sections are written back to back in table order, so each one's byte
    // extent is the gap to the next offset (file end for the last); the
    // checks above make every gap positive. The extent is a size signal for
    // hot-partition splitting; reading records still relies only on offset +
    // count. A plain section must hold at least its record headers.
    for i in 0..sections.len() {
        let next_offset = sections
            .get(i + 1)
            .map_or(file_len, |section| section.offset);
        let extent = next_offset - sections[i].offset;
        if compression == ChunkCompression::None && extent < 12 * u64::from(sections[i].count) {
            return Err(invalid(format!(
                "partition {} holds {} records in {extent} bytes",
                sections[i].partition, sections[i].count
            )));
        }
        sections[i].byte_extent = extent;
    }
    Ok(sections)
}

pub(super) struct ChunkReader {
    reader: ChunkRead,
    pub(super) remaining: u32,
    // Decompressed record bytes read from this chunk, flushed to
    // SORT_MERGE_BYTES on drop so the per-record path stays atomic-free.
    bytes_read: u64,
    pub(super) tile_range: (u64, u64),
}

impl Drop for ChunkReader {
    fn drop(&mut self) {
        SORT_MERGE_BYTES.fetch_add(self.bytes_read, std::sync::atomic::Ordering::Relaxed);
    }
}

type ChunkRecord = (SortKey, Box<[u8]>);

impl ChunkReader {
    pub(super) fn open(
        path: &Path,
        compression: ChunkCompression,
    ) -> Result<Self, ChunkStoreError> {
        let file = File::open(path).map_err(ChunkStoreError::open)?;
        let buf = BufReader::with_capacity(256 * 1024, file);
        let mut reader = match compression {
            ChunkCompression::None => ChunkRead::Plain(buf),
            ChunkCompression::Lz4 => ChunkRead::Lz4(FrameDecoder::new(buf)),
            ChunkCompression::Snappy => ChunkRead::Snappy(snap::read::FrameDecoder::new(buf)),
        };

        let mut buf4 = [0u8; 4];
        reader
            .read_exact(&mut buf4)
            .map_err(ChunkStoreError::read)?;
        let remaining = u32::from_le_bytes(buf4);

        Ok(ChunkReader {
            reader,
            remaining,
            bytes_read: 0,
            tile_range: (0, crate::tiling::PROFILE_ID_END),
        })
    }

    pub(super) fn open_source(
        source: &SortPartitionSource,
        compression: ChunkCompression,
        tile_range: (u64, u64),
    ) -> Result<Self, ChunkStoreError> {
        let mut reader = match &source.section {
            None => Self::open(&source.path, compression),
            Some(section) => {
                let mut file = File::open(&source.path).map_err(ChunkStoreError::open)?;
                file.seek(SeekFrom::Start(section.offset))?;
                let buf = BufReader::with_capacity(256 * 1024, file);
                // Compressed sections are independent frames starting at the
                // section offset; reading exactly `count` records consumes
                // exactly one frame's content.
                let reader = match compression {
                    ChunkCompression::None => ChunkRead::Plain(buf),
                    ChunkCompression::Lz4 => ChunkRead::Lz4(FrameDecoder::new(buf)),
                    ChunkCompression::Snappy => {
                        ChunkRead::Snappy(snap::read::FrameDecoder::new(buf))
                    }
                };
                Ok(ChunkReader {
                    reader,
                    remaining: section.count,
                    bytes_read: 0,
                    tile_range,
                })
            }
        }?;
        reader.tile_range = tile_range;
        Ok(reader)
    }

    fn read_key(&mut self) -> Result<SortKey, ChunkStoreError> {
        let mut buf = [0u8; 8];
        self.reader
            .read_exact(&mut buf)
            .map_err(ChunkStoreError::read)?;
        let key = SortKey::from_raw(u64::from_le_bytes(buf));
        let tile = key.tile().raw();
        let (start, end) = self.tile_range;
        // Check before payload allocation or skipping. Section counts and
        // payload lengths can otherwise carry a reader into another partition.
        if tile < start || tile >= end {
            return Err(ChunkStoreError::new(
                ChunkStoreErrorKind::Corrupt,
                format!("sort chunk tile id {tile} outside source range [{start}, {end})"),
            ));
        }
        Ok(key)
    }

    // Allocates a Vec per record. A reusable buffer was considered but the heap
    // holds only k entries (1-4 chunks for Denmark, ~20 for planet) and records
    // vary in size, so a pool would often reallocate anyway. Not a bottleneck.
    pub(super) fn read_record(&mut self) -> Result<Option<ChunkRecord>, ChunkStoreError> {
        if self.remaining == 0 {
            return Ok(None);
        }

        let key = self.read_key()?;

        let mut buf4 = [0u8; 4];
        self.reader
            .read_exact(&mut buf4)
            .map_err(ChunkStoreError::read)?;
        let data_len = u32::from_le_bytes(buf4);

        let mut data = vec![0u8; data_len as usize].into_boxed_slice();
        self.reader
            .read_exact(&mut data)
            .map_err(ChunkStoreError::read)?;

        self.remaining -= 1;
        self.bytes_read += 12 + u64::from(data_len);
        Ok(Some((key, data)))
    }

    /// Read the next record's header and discard its payload without
    /// allocating. Serves the hot-partition piece pre-scan and the
    /// compressed-source piece skip. Deliberately does NOT feed
    /// `bytes_read`: SORT_MERGE_BYTES accounts merged records, and a
    /// skipped or scanned record is not merged.
    pub(super) fn next_header_discard_payload(
        &mut self,
    ) -> Result<Option<(SortKey, u32)>, ChunkStoreError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let key = self.read_key()?;
        let mut buf4 = [0u8; 4];
        self.reader
            .read_exact(&mut buf4)
            .map_err(ChunkStoreError::read)?;
        let data_len = u32::from_le_bytes(buf4);
        let copied = io::copy(
            &mut Read::by_ref(&mut self.reader).take(u64::from(data_len)),
            &mut io::sink(),
        )
        .map_err(ChunkStoreError::read)?;
        if copied != u64::from(data_len) {
            return Err(ChunkStoreError::new(
                ChunkStoreErrorKind::Corrupt,
                "sort chunk record truncated during payload skip",
            ));
        }
        self.remaining -= 1;
        Ok(Some((key, data_len)))
    }

    /// Decode and discard `n` records. Used to enter a compressed section
    /// mid-stream for a partition piece; frames cannot be seeked into, so
    /// the records before the piece boundary are decompressed and dropped.
    pub(super) fn skip_records(&mut self, n: u64) -> Result<(), ChunkStoreError> {
        for _ in 0..n {
            if self.next_header_discard_payload()?.is_none() {
                return Err(ChunkStoreError::new(
                    ChunkStoreErrorKind::Corrupt,
                    "partition piece skip ran past its section's record count",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::next_random;

    #[test]
    fn chunk_reads_and_scans_enforce_exact_partition_bounds() {
        let dir = tempfile::Builder::new()
            .prefix("chunk-bounds")
            .tempdir_in(".")
            .expect("create test directory");
        for compression in [
            ChunkCompression::None,
            ChunkCompression::Lz4,
            ChunkCompression::Snappy,
        ] {
            for partition in [SORT_PARTITIONS - 2, SORT_PARTITIONS - 1] {
                let (start, end) = partition_tile_range(partition);
                for tile in [start - 1, start, end - 1, end, u64::MAX >> 16] {
                    let records = [SortRecord {
                        key: SortKey::from_raw(tile << 16),
                        data: Box::from([7u8]),
                    }];
                    for multi in [false, true] {
                        let path = dir.path().join("chunk.bin");
                        let ranges = [PartitionRange {
                            partition,
                            start: 0,
                            end: 1,
                        }];
                        write_records(
                            records.as_slice(),
                            multi.then_some(ranges.as_slice()),
                            &path,
                            compression,
                        )
                        .expect("write test chunk");
                        let source = if multi {
                            let sections = read_multi_chunk_sections(&path, compression)
                                .expect("read section table");
                            SortPartitionSource::section(path, sections[0])
                        } else {
                            SortPartitionSource::whole(path, 13)
                        };
                        let valid = tile >= start && tile < end;
                        let mut reader =
                            ChunkReader::open_source(&source, compression, (start, end))
                                .expect("open record reader");
                        let result = reader.read_record();
                        if valid {
                            assert_eq!(
                                result.expect("valid record").expect("one record").0,
                                records[0].key
                            );
                        } else {
                            assert_eq!(
                                result.expect_err("reject foreign tile").kind(),
                                ChunkStoreErrorKind::Corrupt
                            );
                        }
                        let seek = SourceSeek::new(source, compression, partition);
                        let result = seek
                            .cursor()
                            .expect("open scan")
                            .next_header_discard_payload();
                        if valid {
                            assert_eq!(
                                result.expect("valid header").expect("one header").0,
                                records[0].key
                            );
                        } else {
                            assert_eq!(
                                result.expect_err("reject foreign header").kind(),
                                ChunkStoreErrorKind::Corrupt
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn piece_reads_check_narrowed_range_after_compressed_skip() {
        let dir = tempfile::Builder::new()
            .prefix("piece-bounds")
            .tempdir_in(".")
            .expect("create test directory");
        let partition = SORT_PARTITIONS - 1;
        let (start, end) = partition_tile_range(partition);
        for compression in [
            ChunkCompression::None,
            ChunkCompression::Lz4,
            ChunkCompression::Snappy,
        ] {
            let records: Vec<_> = [start, start + 1, end - 1]
                .into_iter()
                .map(|tile| SortRecord {
                    key: SortKey::from_raw(tile << 16),
                    data: Box::from([7u8]),
                })
                .collect();
            let path = dir.path().join("piece.bin");
            write_records(records.as_slice(), None, &path, compression).expect("write chunk");
            let seek =
                SourceSeek::new(SortPartitionSource::whole(path, 39), compression, partition);
            let mut cursor = seek.cursor().expect("open cursor");
            cursor.next_header_discard_payload().expect("skip first");
            let first = cursor.position();
            cursor.next_header_discard_payload().expect("scan second");
            let last = cursor.position();
            let slice = seek
                .slice(first, last)
                .expect("slice source")
                .expect("nonempty");
            let mut reader =
                SortPartitionReader::open_slices(&[slice], compression, (start + 1, end - 1))
                    .expect("open valid piece");
            assert_eq!(
                reader.next().expect("read piece").expect("one record").key,
                records[1].key
            );
            assert!(reader.next().expect("piece end").is_none());
            let slice = seek
                .slice(first, last)
                .expect("slice source")
                .expect("nonempty");
            let err = SortPartitionReader::open_slices(&[slice], compression, (start + 2, end))
                .err()
                .expect("reject out of piece");
            assert_eq!(err.kind(), ChunkStoreErrorKind::Corrupt);
        }
    }

    #[test]
    fn sort_chunk_record_layout_round_trips_keys_and_payloads() {
        let mut seed = 0x6a09_e667_f3bc_c909;
        let mut records = Vec::with_capacity(512);
        for index in 0..512 {
            let key = match index {
                0 => 0,
                1 => u64::MAX,
                2 => 1_u64 << 63,
                3 => 1,
                _ => next_random(&mut seed),
            };
            let length = match index {
                0 => 0,
                1 => 1,
                2 => 255,
                3 => 256,
                _ => usize::try_from(next_random(&mut seed) % 4097)
                    .expect("bounded payload length fits usize"),
            };
            let data: Vec<u8> = (0..length)
                .map(|_| {
                    u8::try_from(next_random(&mut seed) & u64::from(u8::MAX))
                        .expect("masked random byte fits u8")
                })
                .collect();
            records.push(SortRecord {
                key: SortKey::from_raw(key % (crate::tiling::PROFILE_ID_END << 16)),
                data: data.into_boxed_slice(),
            });
        }
        records[4].data = (0..=u8::MAX).collect::<Vec<_>>().into_boxed_slice();

        let dir = tempfile::Builder::new()
            .prefix("chunk-layout")
            .tempdir_in(".")
            .expect("create sort chunk test directory");
        for (case, compression) in [
            ChunkCompression::None,
            ChunkCompression::Lz4,
            ChunkCompression::Snappy,
        ]
        .into_iter()
        .enumerate()
        {
            let path = dir.path().join(format!("records-{case}.bin"));
            write_records(records.as_slice(), None, &path, compression)
                .expect("write sort chunk records");

            let mut reader = ChunkReader::open(&path, compression).expect("open sort chunk");
            assert_eq!(
                reader.remaining,
                u32::try_from(records.len()).expect("test record count fits u32")
            );
            for expected in &records {
                let (key, data) = reader
                    .read_record()
                    .expect("read sort chunk record")
                    .expect("record remains");
                assert_eq!(key, expected.key);
                assert_eq!(data.as_ref(), expected.data.as_ref());
            }
            assert!(
                reader
                    .read_record()
                    .expect("read past last sort chunk record")
                    .is_none()
            );
        }
    }
}
