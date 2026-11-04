use super::*;

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
        self.key == other.key && self.data == other.data && self.chunk_idx == other.chunk_idx
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering: smallest record should come out first from a max-heap.
        record_cmp(other.key, &other.data, self.key, &self.data)
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
    /// Prime the merge heap from already-open (and possibly skipped-into)
    /// chunk readers.
    fn from_chunk_readers(mut chunk_readers: Vec<ChunkReader>) -> io::Result<Self> {
        let mut heap = BinaryHeap::with_capacity(chunk_readers.len());
        for (idx, cr) in chunk_readers.iter_mut().enumerate() {
            if let Some((key, data)) = cr.read_record()? {
                heap.push(HeapEntry {
                    key,
                    data,
                    chunk_idx: idx,
                });
            }
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

    fn new(sources: &[SortPartitionSource], compression: ChunkCompression) -> io::Result<Self> {
        let mut chunk_readers = Vec::with_capacity(sources.len());
        for source in sources {
            chunk_readers.push(ChunkReader::open_source(source, compression)?);
        }
        Self::from_chunk_readers(chunk_readers)
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
#[derive(Clone)]
pub struct SortPartition {
    pub index: usize,
    sources: Vec<SortPartitionSource>,
}

impl SortPartition {
    /// Total on-disk source bytes behind this partition: exact record bytes
    /// for uncompressed chunks, compressed frame bytes otherwise. The
    /// assemble split threshold uses this as a size signal.
    pub fn record_bytes(&self) -> u64 {
        self.sources.iter().map(|source| source.byte_extent).sum()
    }

    pub(crate) fn source_seeks(&self, compression: ChunkCompression) -> Vec<SourceSeek> {
        self.sources
            .iter()
            .cloned()
            .map(|source| SourceSeek::new(source, compression, self.index))
            .collect()
    }
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

    pub(crate) fn open_slices(
        slices: &[SortSourceSlice],
        compression: ChunkCompression,
    ) -> io::Result<Self> {
        let mut chunk_readers = Vec::with_capacity(slices.len());
        for slice in slices {
            let mut reader = ChunkReader::open_source(&slice.source, compression)?;
            if slice.skip_records > 0 {
                reader.skip_records(slice.skip_records)?;
            }
            // Compressed sources start at the frame boundary and discard
            // records before the piece; uncompressed slices seek to the first
            // record and carry a narrowed section count.
            reader.remaining = slice.take_records;
            chunk_readers.push(reader);
        }
        Ok(Self {
            inner: PartitionMergeReader::from_chunk_readers(chunk_readers)?,
        })
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> io::Result<Option<SortRecord>> {
        self.inner.next()
    }
}

/// Manifest of ordered partitions from current chunk files.
/// Unpartitioned names and incompatible split zooms are rejected.
pub struct SortReader {
    partitions: Vec<Vec<SortPartitionSource>>,
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

    /// Read the chunk manifest. Merge cursors open lazily per partition.
    pub(super) fn new(chunk_paths: &[PathBuf], compression: ChunkCompression) -> io::Result<Self> {
        let mut partitions = vec![Vec::new(); SORT_PARTITIONS];
        for path in chunk_paths {
            match parse_chunk_filename(path)?.map(|(_, kind)| kind) {
                Some(ChunkFileKind::Partition(partition)) => {
                    // Preserve eager validation of the whole-file count prefix.
                    let _ = ChunkReader::open(path, compression)?;
                    let byte_extent = fs::metadata(path)?.len().saturating_sub(4);
                    partitions[partition]
                        .push(SortPartitionSource::whole(path.clone(), byte_extent));
                }
                Some(ChunkFileKind::Multi) => {
                    for section in read_multi_chunk_sections(path, compression)? {
                        if section.count > 0 {
                            partitions[section.partition]
                                .push(SortPartitionSource::section(path.clone(), section));
                        }
                    }
                }
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("sort chunk requires a partition suffix: {}", path.display()),
                    ));
                }
            }
        }
        Ok(Self { partitions })
    }

    /// Take the ordered, nonempty partition groups for parallel assembly.
    pub fn take_partitions(&mut self) -> Vec<SortPartition> {
        std::mem::take(&mut self.partitions)
            .into_iter()
            .enumerate()
            .filter(|(_, sources)| !sources.is_empty())
            .map(|(index, sources)| SortPartition { index, sources })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sort::key::{PARTITION_BASES, TILE_ID_BASES};

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

    fn collect_records(mut reader: SortReader, compression: ChunkCompression) -> Vec<SortRecord> {
        let mut out = Vec::new();
        for partition in reader.take_partitions() {
            let mut reader = SortPartitionReader::open(&partition, compression).unwrap();
            while let Some(record) = reader.next().unwrap() {
                out.push(record);
            }
        }
        out
    }

    fn collect_keys(reader: SortReader, compression: ChunkCompression) -> Vec<u64> {
        collect_records(reader, compression)
            .into_iter()
            .map(|record| record.key)
            .collect()
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
            assert_eq!(
                priority_from_key(key),
                priority,
                "priority mismatch for ({tile_id}, {layer}, {priority})"
            );
        }
    }

    #[test]
    fn merge_order_is_chunk_assignment_independent() {
        fn merged(dir: &Path, chunks: &[&[(&[u8], u64)]]) -> Vec<(u64, Vec<u8>)> {
            let mut paths = Vec::new();
            for (idx, chunk) in chunks.iter().enumerate() {
                let mut records: Vec<SortRecord> = chunk
                    .iter()
                    .map(|(data, key)| SortRecord {
                        key: *key,
                        data: Box::from(*data),
                    })
                    .collect();
                let path = chunk_path(dir, idx, partition_from_key(records[0].key));
                write_sorted_chunk(&mut records, &path, ChunkCompression::None).unwrap();
                paths.push(path);
            }
            let reader = SortReader::new(&paths, ChunkCompression::None).unwrap();
            let mut result = Vec::new();
            for record in collect_records(reader, ChunkCompression::None) {
                result.push((record.key, record.data.into_vec()));
            }
            result
        }

        let key = make_sort_key(7, 11, 2);
        let other = make_sort_key(7, 11, 3);
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        let first = merged(
            left.path(),
            &[
                &[(b"c", key), (b"a", key)],
                &[(b"d", key), (b"b", key), (b"z", other)],
            ],
        );
        let second = merged(
            right.path(),
            &[
                &[(b"z", other), (b"b", key)],
                &[(b"d", key)],
                &[(b"a", key), (b"c", key)],
            ],
        );
        assert_eq!(first, second);
        assert_eq!(
            first,
            vec![
                (key, b"a".to_vec()),
                (key, b"b".to_vec()),
                (key, b"c".to_vec()),
                (key, b"d".to_vec()),
                (other, b"z".to_vec()),
            ]
        );
    }

    #[test]
    fn partition_from_key_uses_split_z_hilbert_prefixes() {
        assert_eq!(SORT_PARTITIONS, 136_533);

        // One z7 prefix at z14 spans 4^(14-7) = 16,384 child tile ids.
        let z14_base = TILE_ID_BASES[14];
        let first = partition_from_key(make_sort_key(z14_base, 0, 0));
        let last_same_prefix = partition_from_key(make_sort_key(z14_base + 16_383, 0, 0));
        let next_prefix = partition_from_key(make_sort_key(z14_base + 16_384, 0, 0));
        assert_eq!(first, PARTITION_BASES[14]);
        assert_eq!(last_same_prefix, first);
        assert_eq!(next_prefix, first + 1);
        assert_eq!(
            partition_next_key(first),
            make_sort_key(z14_base + 16_384, 0, 0)
        );
        assert!(make_sort_key(z14_base + 16_383, u8::MAX, u8::MAX) < partition_next_key(first));

        // One z7 prefix at z13 spans 4^(13-7) = 4,096 child tile ids.
        let z13_base = TILE_ID_BASES[13];
        let z13_first = partition_from_key(make_sort_key(z13_base, 0, 0));
        let z13_next = partition_from_key(make_sort_key(z13_base + 4_096, 0, 0));
        assert_eq!(z13_first, PARTITION_BASES[13]);
        assert_eq!(z13_next, z13_first + 1);
        assert_eq!(
            partition_next_key(z13_first),
            make_sort_key(z13_base + 4_096, 0, 0)
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
    fn unsupported_chunk_names_are_rejected() {
        let dir = tempfile::tempdir().expect("create tempdir");
        for name in [
            "chunk_0000.bin",
            "chunk_0000_p227.bin",
            "chunk_0000_z6p00000.bin",
            "chunk_0000_z6m.bin",
            "chunk_0000_z7p999999.bin",
        ] {
            let path = dir.path().join(name);
            assert_eq!(
                parse_chunk_filename(&path).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            fs::write(&path, []).unwrap();
            let error = SortReader::from_dir(dir.path(), Some(1), ChunkCompression::None)
                .err()
                .expect("unsupported chunk name must refuse directory loading");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            fs::remove_file(path).unwrap();
        }
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
        assert_eq!(
            collect_keys(reader, ChunkCompression::None),
            vec![k0, k1, k1b]
        );

        let mut reader = SortReader::from_dir(dir.path(), Some(1), ChunkCompression::None).unwrap();
        let partitions = reader.take_partitions();
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

        let sink = SortWriter::new(dir.path(), 1_000_000, ChunkCompression::None)
            .unwrap()
            .chunk_sink();
        let paths = write_partitioned_payload_chunks(&mut records, &payload, &sink).unwrap();

        assert_eq!(paths.len(), 1);
        assert_eq!(sink.chunk_count(), 1);
        assert_eq!(chunk_kind_for_id(dir.path(), 0), Some(ChunkFileKind::Multi));
        let reader = SortReader::from_dir(dir.path(), Some(1), ChunkCompression::None).unwrap();
        assert_eq!(collect_keys(reader, ChunkCompression::None), vec![k0, k1]);
    }

    fn compressed_multi_round_trip(compression: ChunkCompression) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 1_000_000, compression).unwrap();

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

        let reader = SortReader::from_dir(dir.path(), Some(1), compression).unwrap();
        assert_eq!(collect_keys(reader, compression), vec![k0, k1, k1b]);

        let mut reader = SortReader::from_dir(dir.path(), Some(1), compression).unwrap();
        let partitions = reader.take_partitions();
        assert_eq!(partitions.len(), 2);

        let mut part0 = SortPartitionReader::open(&partitions[0], compression).unwrap();
        let rec = part0.next().unwrap().expect("p0 record");
        assert_eq!(rec.key, k0);
        assert_eq!(&rec.data[..], k0.to_le_bytes().as_slice());
        assert!(part0.next().unwrap().is_none());

        let mut part1 = SortPartitionReader::open(&partitions[1], compression).unwrap();
        assert_eq!(part1.next().unwrap().expect("p1 first").key, k1);
        assert_eq!(part1.next().unwrap().expect("p1 second").key, k1b);
        assert!(part1.next().unwrap().is_none());
    }

    #[test]
    fn lz4_writer_coalesces_partition_sections_into_one_file() {
        compressed_multi_round_trip(ChunkCompression::Lz4);
    }

    #[test]
    fn snappy_writer_coalesces_partition_sections_into_one_file() {
        compressed_multi_round_trip(ChunkCompression::Snappy);
    }

    fn compressed_single_chunk_round_trip(compression: ChunkCompression) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = chunk_path(dir.path(), 0, partition_from_key(make_sort_key(1, 0, 0)));
        let first_key = make_sort_key(1, 0, 0);
        let second_key = make_sort_key(1, 0, 1);
        let small = vec![0x31; 128];
        let large = vec![0x72; COMPRESSED_CHUNK_BUFFER_SIZE + 32];
        let mut records = vec![
            SortRecord {
                key: second_key,
                data: large.clone().into_boxed_slice(),
            },
            SortRecord {
                key: first_key,
                data: small.clone().into_boxed_slice(),
            },
        ];

        write_sorted_chunk(&mut records, &path, compression).unwrap();
        let paths = vec![path];
        let reader = SortReader::new(&paths, compression).unwrap();
        let decoded: Vec<_> = collect_records(reader, compression)
            .into_iter()
            .map(|record| (record.key, record.data.into_vec()))
            .collect();
        assert_eq!(
            decoded,
            vec![(first_key, small.clone()), (second_key, large.clone())]
        );

        let payload_path = chunk_path(dir.path(), 1, partition_from_key(first_key));
        let payload = [large.as_slice(), small.as_slice()].concat();
        let mut payload_records = vec![
            (second_key, 0, large.len()),
            (first_key, large.len(), small.len()),
        ];
        write_sorted_payload_chunk(&mut payload_records, &payload, &payload_path, compression)
            .unwrap();
        let payload_paths = vec![payload_path];
        let reader = SortReader::new(&payload_paths, compression).unwrap();
        let decoded: Vec<_> = collect_records(reader, compression)
            .into_iter()
            .map(|record| (record.key, record.data.into_vec()))
            .collect();
        assert_eq!(decoded, vec![(first_key, small), (second_key, large)]);
    }

    #[test]
    fn lz4_single_chunk_writers_stream_large_payloads() {
        compressed_single_chunk_round_trip(ChunkCompression::Lz4);
    }

    #[test]
    fn snappy_single_chunk_writers_stream_large_payloads() {
        compressed_single_chunk_round_trip(ChunkCompression::Snappy);
    }

    #[test]
    fn lz4_payload_partition_writer_coalesces_sections_into_one_file() {
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

        let sink = SortWriter::new(dir.path(), 1_000_000, ChunkCompression::Lz4)
            .unwrap()
            .chunk_sink();
        let paths = write_partitioned_payload_chunks(&mut records, &payload, &sink).unwrap();

        assert_eq!(paths.len(), 1);
        assert_eq!(chunk_kind_for_id(dir.path(), 0), Some(ChunkFileKind::Multi));
        let reader = SortReader::from_dir(dir.path(), Some(1), ChunkCompression::Lz4).unwrap();
        assert_eq!(collect_keys(reader, ChunkCompression::Lz4), vec![k0, k1]);
    }

    #[test]
    fn multi_chunk_compression_mismatch_fails_loud() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut writer = SortWriter::new(dir.path(), 1_000_000, ChunkCompression::Lz4).unwrap();

        let p0 = PARTITION_BASES[14];
        let p1 = p0 + 1;
        for partition in [p0, p1] {
            let key = make_sort_key(partition_start_tile_id(partition), 0, 0);
            writer
                .push(SortRecord {
                    key,
                    data: Box::from(key.to_le_bytes().as_slice()),
                })
                .unwrap();
        }
        writer.flush().unwrap();

        let err = match SortReader::from_dir(dir.path(), Some(1), ChunkCompression::None) {
            Ok(_) => panic!("mismatched compression must not open"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("compress-sort-chunks"),
            "unexpected error: {err}"
        );
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

        let reader = writer.finish().unwrap();

        // Collect all output records and verify global sort order.
        let mut output_keys: Vec<u64> = Vec::new();
        for record in collect_records(reader, ChunkCompression::None) {
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
            writer.chunk_count() > 1,
            "expected multiple chunks, got {}",
            writer.chunk_count()
        );

        let reader = writer.finish().unwrap();

        let mut output_keys: Vec<u64> = Vec::new();
        for record in collect_records(reader, ChunkCompression::None) {
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

        assert!(reader.take_partitions().is_empty());
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

        let reader = writer.finish().unwrap();

        let mut count = 0u32;
        for record in collect_records(reader, ChunkCompression::None) {
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

        let reader = writer.finish().unwrap();

        // Records should come out sorted by key (ascending: 1, 2, ..., 50).
        let mut prev_key = 0u64;
        let mut count = 0u64;
        for record in collect_records(reader, ChunkCompression::None) {
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
        let reader = writer.finish().unwrap();

        // All 100 records should be readable in sorted order
        assert_eq!(collect_records(reader, ChunkCompression::None).len(), 100);
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
        let keys = collect_keys(reader, ChunkCompression::None);
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

        // New writes should restart at chunk id zero.
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
    fn adopt_sink_written_chunks_merges_records() {
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

        // Concurrent producers use a clone of the store-owned sink. Each
        // producer writes its payload chunk and hands the path to the manifest.
        let sink = writer.chunk_sink();
        let mut payload_a = Vec::new();
        let mut records_a = Vec::new();
        for key in [10u64, 30] {
            let offset = payload_a.len();
            payload_a.extend_from_slice(&key.to_le_bytes());
            records_a.push((key, offset, 8));
        }
        let mut paths =
            write_partitioned_payload_chunks(&mut records_a, &payload_a, &sink).unwrap();

        let mut payload_b = Vec::new();
        let mut records_b = Vec::new();
        for key in [5u64, 50] {
            let offset = payload_b.len();
            payload_b.extend_from_slice(&key.to_le_bytes());
            records_b.push((key, offset, 8));
        }
        paths.extend(write_partitioned_payload_chunks(&mut records_b, &payload_b, &sink).unwrap());
        writer.adopt_chunk_files(paths);
        assert_eq!(
            writer.chunk_count(),
            2,
            "adopted chunks should count immediately"
        );

        // finish flushes the writer buffer with chunk id two.
        let reader = writer.finish().unwrap();
        let keys = collect_keys(reader, ChunkCompression::None);
        assert_eq!(keys, vec![5, 10, 20, 30, 40, 50]);
    }

    #[test]
    fn chunk_sink_avoids_collisions_between_producers_and_writer() {
        // Models phase12: producers and the drain writer share the chunk
        // store's sink, so no two writes claim the same chunk id.
        let dir = tempfile::tempdir().expect("create tempdir");
        // chunk_size 1 forces a flush on every push.
        let mut writer = SortWriter::new(dir.path(), 1, ChunkCompression::None).unwrap();
        let sink = writer.chunk_sink();
        let payload = 10u64.to_le_bytes();
        let mut task_records = vec![(10, 0, payload.len())];
        let task_paths =
            write_partitioned_payload_chunks(&mut task_records, &payload, &sink).unwrap();
        writer.adopt_chunk_files(task_paths);

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

        assert_eq!(writer.chunk_count(), 3, "3 distinct chunks allocated");
        for i in 0..3 {
            assert!(
                chunk_path_for_id(dir.path(), i).is_some(),
                "chunk id {i} missing - a flush collided and overwrote it"
            );
        }

        let reader = writer.finish().unwrap();
        let keys = collect_keys(reader, ChunkCompression::None);
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

        let missing = chunk_path(dir.path(), 100, 0);
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
        let corrupt = chunk_path(dir.path(), 101, 0);
        std::fs::write(&corrupt, [0xAA, 0xBB]).unwrap();
        writer2.adopt_chunk_files(vec![corrupt]);
        let corrupt_err = writer2
            .finish()
            .err()
            .expect("corrupt adopted chunk should fail");
        assert_eq!(corrupt_err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
