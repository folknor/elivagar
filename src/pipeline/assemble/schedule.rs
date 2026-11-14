//! Partition scheduling: the union of sort partitions and artifact-only
//! ranges, hot-partition pieces, the claim window, each partition's reader
//! and encoder pair, and the ordered writer that drains them.

use std::sync::atomic::Ordering;

use crate::archive::PmtilesWriter;
use crate::debug::{WAIT, wait_span};
use crate::shortbread::Layer;
use crate::sort;
use crate::tiling::TileId;

use super::super::{PipelineError, TileContract, TilegenConfig};
use super::artifact::{
    ArtifactCursor, ArtifactPartitionCtx, artifact_run_copy_items, plan_artifact_items,
};
use super::encode::encode_partition_batch;
use super::report::WriteTally;
use super::{EncodedTile, PartitionItem, PendingTile};

/// Partition scheduling is the union of ordinary sort sources and durable
/// ocean-only ranges. The latter deliberately avoids creating an empty merge
/// heap for copy-only work. Hot sort partitions are replaced by consecutive
/// `SortPiece` entries (contiguous tile-id sub-ranges of one partition), so
/// the ordered writer sees them as ordinary extra order slots and drains
/// tiles in the same global Hilbert order as an unsplit run.
enum UnionPartition<'a> {
    Sort(&'a sort::SortPartition),
    SortPiece(SortPiece),
    ArtifactOnly { start: u64, end: u64 },
}

struct SortPiece {
    start_tile: u64,
    end_tile: u64,
    sources: Vec<sort::SortSourceSlice>,
}

/// The layer of a record read back from a chunk. Chunk keys are not
/// validated when decoded, so a corrupt or foreign layer byte is refused
/// here, where the record enters a tile, and never reaches an encoder.
fn record_layer(key: sort::SortKey) -> Result<Layer, PipelineError> {
    key.layer().ok_or_else(|| {
        let (z, x, y) = key.tile().coord();
        PipelineError(format!(
            "corrupt or foreign sort record has unknown layer id {} in tile {z}/{x}/{y}",
            key.layer_byte()
        ))
    })
}

const MAX_PIECES: u64 = 32;
const MAX_BUCKETS: u64 = 4096;
const MAX_SOURCE_SCAN_THREADS: usize = 8;

struct SourceScan {
    positions: Vec<sort::SourcePosition>,
}

fn scan_source_buckets(
    source: &sort::SourceSeek,
    range_start: u64,
    shift: u32,
    bucket_count: usize,
) -> std::io::Result<SourceScan> {
    let mut positions = vec![sort::SourcePosition::default(); bucket_count + 1];
    let mut cursor = source.cursor()?;
    let mut filled = 0usize;
    loop {
        let record_position = cursor.position();
        let Some((key, _data_len)) = cursor.next_header_discard_payload()? else {
            break;
        };
        // The source cursor checked the exact partition range, including
        // the upper bound when the final bucket is only partly occupied.
        let tile_id = key.tile().raw();
        let bucket = usize::try_from((tile_id - range_start) >> shift)
            .map_err(|_| std::io::Error::other("piece scan bucket does not fit usize"))?;
        if bucket >= bucket_count {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "piece scan found tile id {tile_id} outside its partition range in {}",
                    source.path().display()
                ),
            ));
        }
        // Records are key-sorted, so buckets are non-decreasing; fill each
        // bucket start with the source position before its first record.
        while filled <= bucket {
            positions[filled] = record_position;
            filled += 1;
        }
    }
    let end_position = cursor.position();
    while filled <= bucket_count {
        positions[filled] = end_position;
        filled += 1;
    }
    Ok(SourceScan { positions })
}

/// Choose piece cut points as bucket indices: byte quantiles snapped to
/// bucket edges. A bucket is never split, so the fattest achievable piece is
/// bounded by the fattest bucket. Returns `None` when there is nothing to cut.
fn choose_bucket_cuts(
    bucket_bytes: &[u64],
    target_bytes: u64,
    max_pieces: u64,
) -> Option<Vec<usize>> {
    let bucket_count = bucket_bytes.len();
    let total: u64 = bucket_bytes.iter().sum();
    if total == 0 {
        return None;
    }
    let pieces_wanted = total.div_ceil(target_bytes).clamp(2, max_pieces);
    let mut cuts = vec![0usize];
    let mut acc = 0u64;
    let mut next_quantile = 1u64;
    for (bucket, &bytes) in bucket_bytes.iter().enumerate() {
        acc += bytes;
        while next_quantile < pieces_wanted
            && acc.saturating_mul(pieces_wanted) >= total.saturating_mul(next_quantile)
        {
            if bucket + 1 < bucket_count {
                cuts.push(bucket + 1);
            }
            next_quantile += 1;
        }
    }
    cuts.dedup();
    cuts.push(bucket_count);
    if cuts.len() < 3 { None } else { Some(cuts) }
}

#[allow(clippy::too_many_lines)]
fn plan_pieces(
    partition: &sort::SortPartition,
    compression: sort::ChunkCompression,
    target_bytes: u64,
) -> std::io::Result<Option<Vec<SortPiece>>> {
    let (range_start, range_end) = sort::partition_tile_range(partition.index);
    let width = range_end - range_start;
    if width < 2 || target_bytes == 0 {
        return Ok(None);
    }
    let mut shift = 0u32;
    while (width >> shift) > MAX_BUCKETS {
        shift += 1;
    }
    let bucket_count = usize::try_from((width + (1 << shift) - 1) >> shift)
        .map_err(|_| std::io::Error::other("piece bucket count does not fit usize"))?;
    let sources = partition.source_seeks(compression);
    let next_source = std::sync::atomic::AtomicUsize::new(0);
    let scans = std::thread::scope(|scope| -> std::io::Result<Vec<SourceScan>> {
        let worker_count = sources.len().min(MAX_SOURCE_SCAN_THREADS);
        let handles: Vec<_> = (0..worker_count)
            .map(|_| {
                scope.spawn(|| {
                    let mut scans = Vec::new();
                    loop {
                        let source_index = next_source.fetch_add(1, Ordering::Relaxed);
                        let Some(source) = sources.get(source_index) else {
                            break;
                        };
                        scans.push((
                            source_index,
                            scan_source_buckets(source, range_start, shift, bucket_count),
                        ));
                    }
                    scans
                })
            })
            .collect();

        let mut scans: Vec<Option<std::io::Result<SourceScan>>> =
            (0..sources.len()).map(|_| None).collect();
        for handle in handles {
            for (source_index, scan) in handle.join().expect("piece scan thread panicked") {
                scans[source_index] = Some(scan);
            }
        }
        scans
            .into_iter()
            .map(|scan| scan.expect("every piece source was scanned"))
            .collect()
    })?;

    let mut bucket_bytes = vec![0u64; bucket_count];
    for scan in &scans {
        for (bucket, bytes) in bucket_bytes.iter_mut().enumerate() {
            *bytes += scan.positions[bucket + 1].bytes - scan.positions[bucket].bytes;
        }
    }
    let Some(cuts) = choose_bucket_cuts(&bucket_bytes, target_bytes, MAX_PIECES) else {
        return Ok(None);
    };

    let mut pieces = Vec::with_capacity(cuts.len() - 1);
    for pair in cuts.windows(2) {
        let (cut_start, cut_end) = (pair[0], pair[1]);
        let start_tile = range_start + ((cut_start as u64) << shift);
        let end_tile = range_end.min(range_start + ((cut_end as u64) << shift));
        let mut piece_sources = Vec::new();
        for (source, scan) in sources.iter().zip(&scans) {
            let start = scan.positions[cut_start];
            let end = scan.positions[cut_end];
            if let Some(source_slice) = source.slice(start, end)? {
                piece_sources.push(source_slice);
            }
        }
        pieces.push(SortPiece {
            start_tile,
            end_tile,
            sources: piece_sources,
        });
    }
    Ok(Some(pieces))
}

pub(super) struct AssembleCore {
    pub(super) features_read: u64,
    pub(super) pmtiles: PmtilesWriter,
    pub(super) tally: WriteTally,
    pub(super) max_batch_bytes: usize,
    pub(super) reader_ns: u64,
    pub(super) reader_total_ns: u64,
    pub(super) partition_workers: usize,
    pub(super) partition_count: usize,
}

pub(super) struct PartitionBatch {
    order: usize,
    batch_index: usize,
    is_last: bool,
    features_read: u64,
    max_batch_bytes: usize,
    reader_ns: u64,
    pub(super) items: Vec<PartitionItem>,
}

pub(super) struct PartitionBatchMeta {
    pub(super) order: usize,
    pub(super) batch_index: usize,
    pub(super) is_last: bool,
    pub(super) features_read: u64,
    pub(super) max_batch_bytes: usize,
    pub(super) reader_ns: u64,
}

#[derive(Default)]
struct PendingPartitionBatches {
    batches: std::collections::BTreeMap<usize, PartitionBatch>,
}

fn batch_encoded_bytes(batch: &PartitionBatch) -> usize {
    batch
        .items
        .iter()
        .filter_map(|item| match item {
            PartitionItem::Encoded(tile) => {
                Some(std::mem::size_of::<EncodedTile>() + tile.compressed.len())
            }
            // Artifact run copies borrow their payload from the mmap until the
            // writer reaches them, so they do not consume parking budget.
            PartitionItem::RunCopy { .. } => None,
        })
        .sum()
}

#[allow(clippy::too_many_lines)]
pub(super) fn phase_assemble_partitions(
    partitions: &[sort::SortPartition],
    mut pmtiles: PmtilesWriter,
    config: &TilegenConfig,
    resources: &crate::resources::Resources,
    ocean: Option<&crate::ocean::OceanTiles>,
) -> Result<AssembleCore, PipelineError> {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::mpsc;

    let split_target = resources.assemble_split_target;
    let park_budget = resources.assemble_park_budget;
    let union = partition_union(partitions, ocean);
    // H8b: split hot partitions into tile-range pieces. The z7 partition
    // scheme bounds MOST partitions well, but one dense z14-block prefix
    // can put hundreds of encoded MB behind a single ordered slot and one
    // serial merge reader (germany's Berlin partition: 297 MB encoded, 10%
    // of the archive, 3.6s of serial read; the writer starved 12s on
    // ordered batches while the claim window never bound). Pieces make the
    // straggler's read AND its ordered drain incremental. The threshold is
    // 2x the piece target so a split always yields at least two real
    // pieces; the target is overridable for A/Bs and for forcing splits on
    // small extracts (the bit-identity gate builds denmark split and
    // unsplit and compares archives with cmp).
    let mut split_partitions = 0usize;
    let mut split_pieces = 0usize;
    let mut split_union = Vec::with_capacity(union.len());
    for entry in union {
        match entry {
            UnionPartition::Sort(partition)
                if partition.record_bytes() > split_target.saturating_mul(2) =>
            {
                match plan_pieces(partition, config.compress_sort_chunks, split_target)
                    .map_err(PipelineError::from)?
                {
                    Some(pieces) => {
                        split_partitions += 1;
                        split_pieces += pieces.len();
                        split_union.extend(pieces.into_iter().map(UnionPartition::SortPiece));
                    }
                    None => split_union.push(UnionPartition::Sort(partition)),
                }
            }
            entry => split_union.push(entry),
        }
    }
    let union = split_union;
    crate::debug::emit_counter_usize("assemble_split_partitions", split_partitions);
    crate::debug::emit_counter_usize("assemble_split_pieces", split_pieces);
    let partition_count = union.len();
    if partition_count == 0 {
        return Ok(AssembleCore {
            features_read: 0,
            pmtiles,
            tally: WriteTally::default(),
            max_batch_bytes: 0,
            reader_ns: 0,
            reader_total_ns: 0,
            partition_workers: 0,
            partition_count: 0,
        });
    }

    let worker_count = resources.assemble_workers(partition_count);
    let next_job = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<Result<PartitionBatch, PipelineError>>();
    // Claim window: the writer consumes partitions in order (PMTiles payload
    // is written clustered in Hilbert order), so batches from partitions
    // ahead of next_write park in `pending` until their turn. Without a
    // bound, workers racing ahead of a dense straggler partition accumulate
    // every finished batch in RAM - measured 19.5 GB of encoded tiles on the
    // NA locations run (15442 partitions, 4 workers). Workers may not START
    // partition N until N < next_write + window, bounding parked output to
    // ~window partitions' worth by construction. The claimer of next_write
    // itself is always inside the window, so progress is guaranteed.
    let claim_window = worker_count * 2;
    // Byte-budgeted claim relaxation: the partition-count window alone parks
    // workers behind every straggler even when almost nothing is held in RAM
    // (measured at 8 workers on NA locations: claim-window wait 83.6% of
    // assemble wall while parked HWM stayed under 1.3 GB of a multi-GB
    // budget). Workers may claim ANY distance ahead while the writer's
    // parked bytes sit under the budget; past it, claims collapse back to
    // the tight window. Progress guarantee unchanged - the claimer of
    // next_write is always inside the window. RAM bound: parked bytes stop
    // growing once over budget except for the <= worker_count partitions
    // already claimed, so worst case is budget + workers x fattest
    // partition - the same exposure class as the count window.
    struct ClaimState {
        next_write: usize,
        parked_bytes: usize,
    }
    let write_progress = (
        std::sync::Mutex::new(ClaimState {
            next_write: 0,
            parked_bytes: 0,
        }),
        std::sync::Condvar::new(),
    );
    let write_progress_ref = &write_progress;

    let compression = config.compress_sort_chunks;
    let contract = TileContract::from_config(config);
    let assemble_budget = resources.assemble_batch;
    let artifact_ctx = ocean.map(ArtifactPartitionCtx::new).transpose()?;

    let mut features_read: u64 = 0;
    let mut tally = WriteTally::default();
    let mut max_batch_bytes: usize = 0;
    let mut reader_max_ns: u64 = 0;
    let mut reader_total_ns: u64 = 0;
    let mut max_parked_bytes: usize = 0;
    let mut current_partition_encoded: usize = 0;
    let mut max_partition_encoded: usize = 0;

    let scope_result: Result<(), PipelineError> = std::thread::scope(|s| {
        for _ in 0..worker_count {
            let tx = tx.clone();
            let artifact_ctx = artifact_ctx.as_ref();
            let union_ref = &union;
            let next_job_ref = &next_job;
            let stop_ref = &stop;
            s.spawn(move || {
                loop {
                    if stop_ref.load(Ordering::Relaxed) {
                        break;
                    }
                    let order = next_job_ref.fetch_add(1, Ordering::Relaxed);
                    if order >= union_ref.len() {
                        break;
                    }
                    {
                        let _wait = wait_span(&WAIT.assemble_claim_window);
                        let mut state = write_progress_ref
                            .0
                            .lock()
                            .expect("assemble write progress lock");
                        while order >= state.next_write + claim_window
                            && state.parked_bytes >= park_budget
                            && !stop_ref.load(Ordering::Relaxed)
                        {
                            state = write_progress_ref
                                .1
                                .wait(state)
                                .expect("assemble write progress wait");
                        }
                    }
                    if stop_ref.load(Ordering::Relaxed) {
                        break;
                    }
                    let result = match &union_ref[order] {
                        UnionPartition::Sort(partition) => {
                            sort::SortPartitionReader::open(partition, compression)
                                .map_err(PipelineError::from)
                                .and_then(|reader| {
                                    read_encode_partition(
                                        order,
                                        reader,
                                        sort::partition_tile_range(partition.index),
                                        contract,
                                        assemble_budget,
                                        artifact_ctx,
                                        &tx,
                                    )
                                })
                        }
                        UnionPartition::SortPiece(piece) => sort::SortPartitionReader::open_slices(
                            &piece.sources,
                            compression,
                            (piece.start_tile, piece.end_tile),
                        )
                        .map_err(PipelineError::from)
                        .and_then(|reader| {
                            read_encode_partition(
                                order,
                                reader,
                                (piece.start_tile, piece.end_tile),
                                contract,
                                assemble_budget,
                                artifact_ctx,
                                &tx,
                            )
                        }),
                        UnionPartition::ArtifactOnly { start, end } => artifact_ctx
                            .ok_or_else(|| {
                                PipelineError(
                                    "artifact-only partition without an ocean artifact".to_string(),
                                )
                            })
                            .and_then(|artifact| {
                                let items = artifact_run_copy_items(*start, *end, artifact)?;
                                send_partition_batch(
                                    &tx,
                                    PartitionBatch {
                                        order,
                                        batch_index: 0,
                                        is_last: true,
                                        features_read: 0,
                                        max_batch_bytes: 0,
                                        reader_ns: 0,
                                        items,
                                    },
                                )
                            }),
                    };
                    if result.is_err() {
                        stop_ref.store(true, Ordering::Relaxed);
                    }
                    if let Err(err) = result
                        && tx.send(Err(err)).is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(tx);

        let mut pending = BTreeMap::<usize, PendingPartitionBatches>::new();
        let mut next_write = 0usize;
        let mut next_batch = 0usize;
        let mut current_partition_reader_ns = 0u64;
        // Writer body in an inner closure so every exit path (including `?`
        // errors) falls through to the stop+notify below - workers parked on
        // the claim-window condvar must always be woken before scope join.
        let writer_result: Result<(), PipelineError> = (|| {
            loop {
                let result = {
                    let _wait = wait_span(&WAIT.assemble_partition_batch);
                    match rx.recv() {
                        Ok(result) => result,
                        Err(_) => break,
                    }
                };
                let batch = result?;
                if batch.order >= partition_count {
                    return Err(PipelineError(format!(
                        "assemble partition batch has invalid order {} of {partition_count}",
                        batch.order
                    )));
                }
                // RAM-ledger instrumentation: bytes parked in the pending map
                // waiting for their partition's turn, and per-partition encoded
                // totals - the two candidate holders of assemble's measured
                // 17-19 GB plateau (live under all three allocators). The
                // parked total lives inside the claim mutex: workers consult
                // it for the byte-budgeted claim rule.
                {
                    let mut state = write_progress
                        .0
                        .lock()
                        .expect("assemble write progress lock");
                    state.parked_bytes += batch_encoded_bytes(&batch);
                    max_parked_bytes = max_parked_bytes.max(state.parked_bytes);
                }
                let batch_order = batch.order;
                let batch_index = batch.batch_index;
                let state = pending.entry(batch_order).or_default();
                if state.batches.insert(batch_index, batch).is_some() {
                    return Err(PipelineError(format!(
                        "duplicate assemble partition batch {batch_index} for partition {batch_order}"
                    )));
                }

                while pending
                    .get(&next_write)
                    .is_some_and(|state| state.batches.contains_key(&next_batch))
                {
                    let state = pending
                        .get_mut(&next_write)
                        .expect("ready partition exists");
                    let batch = state
                        .batches
                        .remove(&next_batch)
                        .expect("ready batch exists");
                    let batch_is_last = batch.is_last;
                    let drained_bytes = batch_encoded_bytes(&batch);
                    {
                        let mut state = write_progress
                            .0
                            .lock()
                            .expect("assemble write progress lock");
                        state.parked_bytes = state.parked_bytes.saturating_sub(drained_bytes);
                        // Parked bytes dropped - claims blocked on the byte
                        // budget may proceed.
                        write_progress.1.notify_all();
                    }
                    current_partition_encoded += drained_bytes;
                    features_read += batch.features_read;
                    max_batch_bytes = max_batch_bytes.max(batch.max_batch_bytes);
                    reader_total_ns += batch.reader_ns;
                    current_partition_reader_ns += batch.reader_ns;
                    for item in batch.items {
                        match item {
                            PartitionItem::Encoded(tile) => {
                                tally.write_encoded(&mut pmtiles, &tile)?;
                            }
                            PartitionItem::RunCopy {
                                tile_id,
                                run_length,
                                blob,
                            } => {
                                let ocean = ocean.ok_or_else(|| {
                                    PipelineError(
                                        "artifact run copy without an ocean artifact".to_string(),
                                    )
                                })?;
                                tally.write_run_copy(
                                    &mut pmtiles,
                                    tile_id,
                                    run_length,
                                    &ocean.raw_blob(blob)?,
                                )?;
                            }
                        }
                    }
                    if batch_is_last {
                        if !state.batches.is_empty() {
                            return Err(PipelineError(format!(
                                "assemble partition {next_write} received batches after final marker"
                            )));
                        }
                        reader_max_ns = reader_max_ns.max(current_partition_reader_ns);
                        current_partition_reader_ns = 0;
                        max_partition_encoded =
                            max_partition_encoded.max(current_partition_encoded);
                        current_partition_encoded = 0;
                        pending.remove(&next_write);
                        next_write += 1;
                        next_batch = 0;
                        // Open the claim window one partition further.
                        let mut state = write_progress
                            .0
                            .lock()
                            .expect("assemble write progress lock");
                        state.next_write = next_write;
                        write_progress.1.notify_all();
                    } else {
                        next_batch += 1;
                    }
                }
            }

            if next_write != partition_count {
                return Err(PipelineError(format!(
                    "assemble partition worker stopped after {next_write} of {partition_count} partitions"
                )));
            }
            Ok(())
        })();

        // Wake any worker parked on the claim window, success or error.
        stop.store(true, Ordering::Relaxed);
        {
            let _guard = write_progress
                .0
                .lock()
                .expect("assemble write progress lock");
            write_progress.1.notify_all();
        }
        writer_result
    });
    scope_result?;

    crate::debug::emit_counter_usize("assemble_parked_bytes_hwm", max_parked_bytes);
    crate::debug::emit_counter_usize("assemble_partition_encoded_max", max_partition_encoded);

    Ok(AssembleCore {
        features_read,
        pmtiles,
        tally,
        max_batch_bytes,
        reader_ns: reader_max_ns,
        reader_total_ns,
        partition_workers: worker_count,
        partition_count,
    })
}

/// One raw batch handed from a partition's merge reader to its encoder
/// thread: the batch metadata and the pending tiles. The encoder's
/// `ArtifactCursor` holds the range end that artifact gap filling runs to
/// once `meta.is_last` arrives.
type RawPartitionBatch = (PartitionBatchMeta, Vec<PendingTile>);

#[allow(clippy::too_many_lines)]
fn read_encode_partition(
    order: usize,
    mut reader: sort::SortPartitionReader,
    tile_range: (u64, u64),
    contract: TileContract,
    assemble_budget: usize,
    artifact: Option<&ArtifactPartitionCtx<'_>>,
    tx: &std::sync::mpsc::Sender<Result<PartitionBatch, PipelineError>>,
) -> Result<(), PipelineError> {
    const BATCH_SIZE: usize = 4096;

    // For a whole partition this is its tile range; for a piece, the piece's
    // sub-range - artifact gap filling covers exactly this span, so pieces
    // of one partition tile-partition its artifact coverage too.
    let (range_start, partition_end) = tile_range;

    // Read/encode overlap (H8): the worker thread stays the serial merge
    // reader; a scoped encoder thread runs the rayon encode (which appends
    // the artifact ocean) + artifact planning + send. sync_channel(1) lets
    // the reader pull batch N+1 while batch N encodes - previously the
    // reader sat idle through every encode. RAM bound: one extra raw batch
    // (assemble_budget, 32 MB default) in flight per worker. Batch order
    // within the partition is preserved by channel FIFO; the artifact
    // cursor lives on the encoder thread where batches stay sequential.
    // Error cascade: an encoder error drops batch_rx, the reader's send
    // fails and it stops; a reader error drops batch_tx and the encoder
    // drains out - either error surfaces after the join.
    std::thread::scope(|scope| -> Result<(), PipelineError> {
        let (batch_tx, batch_rx) = std::sync::mpsc::sync_channel::<RawPartitionBatch>(1);
        let writer_tx = tx.clone();
        let encoder = scope.spawn(move || -> Result<(), PipelineError> {
            let mut artifact_cursor = ArtifactCursor {
                next: range_start,
                end: partition_end,
            };
            while let Ok((meta, batch)) = batch_rx.recv() {
                encode_and_send_partition_batch(
                    &writer_tx,
                    contract,
                    &meta,
                    &batch,
                    artifact,
                    &mut artifact_cursor,
                )?;
            }
            Ok(())
        });

        let read_result: Result<(), PipelineError> = (|| {
            let mut batch_features_read: u64 = 0;
            let mut batch: Vec<PendingTile> = Vec::with_capacity(BATCH_SIZE);
            // No tile yet: an empty feature list marks it, since every
            // started tile holds at least the record that started it.
            let mut current = PendingTile {
                tile_id: TileId::ZERO,
                features: Vec::new(),
            };
            let mut current_tile_bytes: usize = 0;
            let mut batch_bytes: usize = 0;
            let mut batch_index = 0usize;
            let mut read_started = std::time::Instant::now();

            loop {
                let record = reader.next()?;
                let Some(r) = record else {
                    if !current.features.is_empty() {
                        batch_bytes += 32 + current_tile_bytes;
                        batch.push(current);
                    }
                    let mut reader_ns = 0;
                    add_reader_elapsed(&mut reader_ns, read_started);
                    let meta = PartitionBatchMeta {
                        order,
                        batch_index,
                        is_last: true,
                        features_read: batch_features_read,
                        max_batch_bytes: if batch.is_empty() { 0 } else { batch_bytes },
                        reader_ns,
                    };
                    let send_result = {
                        let _wait = wait_span(&WAIT.assemble_encode_backpressure);
                        batch_tx.send((meta, batch))
                    };
                    drop(send_result); // encoder may have exited on error
                    break;
                };
                batch_features_read += 1;

                let tile_id = r.key.tile();
                let layer = record_layer(r.key)?;
                if tile_id != current.tile_id || current.features.is_empty() {
                    if !current.features.is_empty() {
                        batch_bytes += 32 + current_tile_bytes;
                        batch.push(current);
                        if batch.len() >= BATCH_SIZE || batch_bytes >= assemble_budget {
                            let mut reader_ns = 0;
                            add_reader_elapsed(&mut reader_ns, read_started);
                            let meta = PartitionBatchMeta {
                                order,
                                batch_index,
                                is_last: false,
                                features_read: batch_features_read,
                                max_batch_bytes: batch_bytes,
                                reader_ns,
                            };
                            let send_result = {
                                let _wait = wait_span(&WAIT.assemble_encode_backpressure);
                                batch_tx.send((meta, batch))
                            };
                            if send_result.is_err() {
                                // Encoder exited on error; its error surfaces
                                // at the join below.
                                return Ok(());
                            }
                            batch_index += 1;
                            batch_features_read = 0;
                            batch = Vec::with_capacity(BATCH_SIZE);
                            batch_bytes = 0;
                            read_started = std::time::Instant::now();
                        }
                    }
                    current = PendingTile {
                        tile_id,
                        features: Vec::new(),
                    };
                    current_tile_bytes = 0;
                }
                let data_len = r.data.len();
                current.features.push((layer, r.data));
                current_tile_bytes += 32 + data_len;
            }
            Ok(())
        })();

        drop(batch_tx);
        let encode_result = encoder.join().expect("assemble partition encoder panicked");
        read_result?;
        encode_result
    })
}

pub(super) fn encode_and_send_partition_batch(
    tx: &std::sync::mpsc::Sender<Result<PartitionBatch, PipelineError>>,
    contract: TileContract,
    meta: &PartitionBatchMeta,
    pending_tiles: &[PendingTile],
    artifact: Option<&ArtifactPartitionCtx<'_>>,
    artifact_cursor: &mut ArtifactCursor,
) -> Result<(), PipelineError> {
    let encoded_tiles = if pending_tiles.is_empty() {
        Vec::new()
    } else {
        encode_partition_batch(pending_tiles, contract, artifact)?
    };
    let items = if let Some(artifact) = artifact {
        plan_artifact_items(artifact, artifact_cursor, encoded_tiles, meta.is_last)?
    } else {
        encoded_tiles
            .into_iter()
            .map(PartitionItem::Encoded)
            .collect()
    };
    send_partition_batch(
        tx,
        PartitionBatch {
            order: meta.order,
            batch_index: meta.batch_index,
            is_last: meta.is_last,
            features_read: meta.features_read,
            max_batch_bytes: meta.max_batch_bytes,
            reader_ns: meta.reader_ns,
            items,
        },
    )
}

fn partition_union<'a>(
    partitions: &'a [sort::SortPartition],
    ocean: Option<&crate::ocean::OceanTiles>,
) -> Vec<UnionPartition<'a>> {
    let Some(ocean) = ocean else {
        return partitions.iter().map(UnionPartition::Sort).collect();
    };
    let mut by_index = std::collections::BTreeMap::new();
    for partition in partitions {
        by_index.insert(partition.index, partition);
    }
    let mut union = Vec::new();
    for index in 0..sort::SORT_PARTITIONS {
        let (start, end) = sort::partition_tile_range(index);
        if let Some(partition) = by_index.get(&index) {
            union.push(UnionPartition::Sort(partition));
        } else if !ocean.runs_in(start, end).is_empty() {
            union.push(UnionPartition::ArtifactOnly { start, end });
        }
    }
    union
}

fn send_partition_batch(
    tx: &std::sync::mpsc::Sender<Result<PartitionBatch, PipelineError>>,
    batch: PartitionBatch,
) -> Result<(), PipelineError> {
    tx.send(Ok(batch))
        .map_err(|_| PipelineError("assemble partition receiver stopped".to_string()))
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
fn add_reader_elapsed(reader_ns: &mut u64, started: std::time::Instant) {
    *reader_ns += started.elapsed().as_nanos() as u64;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tile(raw: u64) -> TileId {
        TileId::new(raw).unwrap()
    }

    fn key(tile: TileId, layer: u8, rank: u8) -> sort::SortKey {
        sort::SortKey::new(tile, Layer::ALL[usize::from(layer)], rank)
    }

    #[test]
    fn records_with_an_unknown_layer_byte_are_refused_at_read_back() {
        let forged = sort::SortKey::from_raw((TileId::at(3, 4, 5).raw() << 16) | (255 << 8));
        let error = record_layer(forged).expect_err("layer byte 255 names no layer");
        assert!(
            error.0.contains("unknown layer id 255 in tile 3/4/5"),
            "{}",
            error.0
        );
        assert_eq!(
            record_layer(key(TileId::at(3, 4, 5), Layer::Ocean as u8, 0)).unwrap(),
            Layer::Ocean
        );

        // End to end: the forged record goes through a chunk file and the
        // partition reader, and assembly fails with the diagnostic.
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            sort::SortWriter::new(dir.path(), 1_000_000, sort::ChunkCompression::None).unwrap();
        writer
            .push(sort::SortRecord {
                key: forged,
                data: Box::from([0u8; 16].as_slice()),
            })
            .unwrap();
        let mut reader = writer.finish().unwrap();
        let partitions = reader.take_partitions();
        let partition = &partitions[0];
        let source =
            sort::SortPartitionReader::open(partition, sort::ChunkCompression::None).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let contract = TileContract::Mvt {
            compression: super::super::super::TileCompression::Gzip,
            level: 6,
        };
        let error = read_encode_partition(
            0,
            source,
            sort::partition_tile_range(partition.index),
            contract,
            1 << 20,
            None,
            &tx,
        )
        .expect_err("assembly refuses the forged layer byte");
        assert!(
            error.0.contains("unknown layer id 255 in tile 3/4/5"),
            "{}",
            error.0
        );
    }

    /// Records spread across one z14-block partition with a deliberately hot
    /// tile band, plus neighbor-partition records so chunks carry multiple
    /// sections. A leading flush forces a whole-file source as well.
    fn push_piece_fixture(writer: &mut sort::SortWriter, partition: usize) {
        let base = sort::partition_start_tile_id(partition).raw();
        let neighbor_base = sort::partition_start_tile_id(partition + 1).raw();
        for i in 0..50u64 {
            let tile_id = base + i * 16 + 3;
            let mut data = vec![0u8; 24];
            data[..8].copy_from_slice(&tile_id.to_le_bytes());
            data[8] = 0xEE;
            writer
                .push(sort::SortRecord {
                    key: key(tile(tile_id), 6, 1),
                    data: data.into_boxed_slice(),
                })
                .unwrap();
        }
        writer.flush().unwrap();
        for i in 0..2048u64 {
            let tile_id = base + i * 8;
            let payload_len = if (1000..1100).contains(&i) {
                512
            } else {
                8 + usize::try_from(i % 37).unwrap()
            };
            let mut data = vec![0u8; payload_len];
            data[..8].copy_from_slice(&tile_id.to_le_bytes());
            #[allow(clippy::cast_possible_truncation)]
            let layer = (i % 5) as u8;
            writer
                .push(sort::SortRecord {
                    key: key(tile(tile_id), layer, 0),
                    data: data.into_boxed_slice(),
                })
                .unwrap();
            if i % 3 == 0 {
                writer
                    .push(sort::SortRecord {
                        key: key(tile(neighbor_base + (i % 16)), 0, 0),
                        data: Box::from(i.to_le_bytes().as_slice()),
                    })
                    .unwrap();
            }
        }
    }

    #[test]
    fn partition_pieces_concatenate_to_whole_partition() {
        for compression in [sort::ChunkCompression::None, sort::ChunkCompression::Lz4] {
            let dir = tempfile::tempdir().unwrap();
            let mut writer = sort::SortWriter::new(dir.path(), 16 * 1024, compression).unwrap();
            let first_tile = TileId::at(14, 0, 0);
            let partition_index = key(first_tile, 0, 0).partition() + 5;
            push_piece_fixture(&mut writer, partition_index);
            writer.flush().unwrap();
            let mut reader = writer.finish().unwrap();
            let partitions = reader.take_partitions();
            let target = partitions
                .iter()
                .find(|candidate| candidate.index == partition_index)
                .expect("fixture partition present");

            let mut whole = Vec::new();
            let mut whole_bytes = 0u64;
            let mut whole_reader = sort::SortPartitionReader::open(target, compression).unwrap();
            while let Some(record) = whole_reader.next().unwrap() {
                whole_bytes += 12 + record.data.len() as u64;
                whole.push((record.key, record.data));
            }
            if compression == sort::ChunkCompression::None {
                assert_eq!(
                    target.record_bytes(),
                    whole_bytes,
                    "uncompressed extents are exact record bytes"
                );
            }

            let pieces = plan_pieces(target, compression, 8 * 1024)
                .unwrap()
                .expect("fixture is large enough to split");
            assert!(pieces.len() >= 2, "got {} pieces", pieces.len());
            let (range_start, range_end) = sort::partition_tile_range(partition_index);
            assert_eq!(pieces.first().unwrap().start_tile, range_start);
            assert_eq!(pieces.last().unwrap().end_tile, range_end);

            let mut concat = Vec::new();
            let mut prev_end = range_start;
            for piece in &pieces {
                assert_eq!(
                    piece.start_tile, prev_end,
                    "pieces must tile the partition range"
                );
                prev_end = piece.end_tile;
                let mut piece_reader = sort::SortPartitionReader::open_slices(
                    &piece.sources,
                    compression,
                    (piece.start_tile, piece.end_tile),
                )
                .unwrap();
                while let Some(record) = piece_reader.next().unwrap() {
                    let tile_id = record.key.tile().raw();
                    assert!(
                        tile_id >= piece.start_tile && tile_id < piece.end_tile,
                        "record tile {tile_id} escaped piece [{}, {})",
                        piece.start_tile,
                        piece.end_tile
                    );
                    concat.push((record.key, record.data));
                }
            }
            assert_eq!(
                concat.len(),
                whole.len(),
                "piece union drops or duplicates records"
            );
            for (got, want) in concat.iter().zip(&whole) {
                assert_eq!(got.0, want.0);
                assert_eq!(got.1, want.1);
            }
        }
    }

    #[test]
    fn plan_pieces_refuses_unsplittable_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            sort::SortWriter::new(dir.path(), 1_000_000, sort::ChunkCompression::None).unwrap();
        let z7_tile = TileId::at(7, 0, 0);
        let z7_key = key(z7_tile, 0, 0);
        writer
            .push(sort::SortRecord {
                key: z7_key,
                data: Box::from(z7_key.raw().to_le_bytes().as_slice()),
            })
            .unwrap();
        let z14_tile = TileId::at(14, 0, 0);
        let z14_key = key(z14_tile, 0, 0);
        writer
            .push(sort::SortRecord {
                key: z14_key,
                data: Box::from(z14_key.raw().to_le_bytes().as_slice()),
            })
            .unwrap();
        let mut reader = writer.finish().unwrap();
        let partitions = reader.take_partitions();
        let z7_partition = partitions
            .iter()
            .find(|partition| partition.index == z7_key.partition())
            .unwrap();
        let z14_partition = partitions
            .iter()
            .find(|partition| partition.index == z14_key.partition())
            .unwrap();
        assert!(
            plan_pieces(z7_partition, sort::ChunkCompression::None, 1)
                .unwrap()
                .is_none()
        );

        assert!(choose_bucket_cuts(&[0, 0], 1, MAX_PIECES).is_none());
        assert!(
            plan_pieces(z14_partition, sort::ChunkCompression::None, 0)
                .unwrap()
                .is_none()
        );
    }
}
