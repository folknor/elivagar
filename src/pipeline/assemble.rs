use std::sync::atomic::{AtomicU64, Ordering};

use crate::debug::{WAIT, emit_counter_u64, emit_counter_usize, wait_span};
use crate::geometry;
use crate::mlt;
use crate::mvt::{self, GeomType, LayerBuilder};
use crate::pmtiles_writer::{
    self, PmtilesConfig, PmtilesWriter, TileDataCompression, TileDataFormat,
};
use crate::shortbread::{self, Layer};
use crate::sort;
use crate::wire_format::add_feature_to_layer;
use pbfhogg::ElementReader;

use super::stats::{TileSizeDiagnostics, record_tile_size_diagnostics};
use super::{PipelineError, TileCompression, TilePayloadFormat, TilegenConfig};

// ---------------------------------------------------------------------------
// Phase 4: Tile assembly + PMTiles write
// ---------------------------------------------------------------------------

/// A tile's features collected from the sort reader, ready for parallel encoding.
pub(super) struct PendingTile {
    pub(super) tile_id: u64,
    pub(super) features: Vec<(u8, Box<[u8]>)>, // (layer_idx, feature_data)
}
const _: () = assert!(std::mem::size_of::<PendingTile>() == 32);

/// An encoded + gzip-compressed tile ready for writing to PMTiles.
pub(super) struct EncodedTile {
    pub(super) tile_id: u64,
    pub(super) compressed: Vec<u8>,
}

/// Ordered work scheduled into the single PMTiles writer. `RunCopy` keeps a
/// shared artifact payload out of worker batches until the writer needs it.
pub(super) enum PartitionItem {
    Encoded(EncodedTile),
    RunCopy {
        tile_id: u64,
        run_length: u32,
        offset: u64,
        length: u32,
    },
}

/// Partition scheduling is the union of ordinary sort sources and durable
/// ocean-only ranges. The latter deliberately avoids creating an empty merge
/// heap for copy-only work.
enum UnionPartition<'a> {
    Sort(&'a sort::SortPartition),
    ArtifactOnly { start: u64, end: u64 },
}
const _: () = assert!(std::mem::size_of::<EncodedTile>() == 32);

struct AssembleCore {
    features_read: u64,
    tiles_written: u64,
    pmtiles: PmtilesWriter,
    tiles_per_zoom: [u64; 15],
    unique_per_zoom: [u64; 15],
    bytes_per_zoom: [u64; 15],
    max_batch_bytes: usize,
    size_diag: TileSizeDiagnostics,
    reader_ns: u64,
    reader_total_ns: Option<u64>,
    partition_workers: Option<usize>,
    partition_count: Option<usize>,
}

struct PartitionBatch {
    order: usize,
    batch_index: usize,
    is_last: bool,
    features_read: u64,
    max_batch_bytes: usize,
    reader_ns: u64,
    items: Vec<PartitionItem>,
}

struct PartitionBatchMeta {
    order: usize,
    batch_index: usize,
    is_last: bool,
    features_read: u64,
    max_batch_bytes: usize,
    reader_ns: u64,
}

struct PartitionEncodeCtx<'a> {
    compression_level: u32,
    tile_format: TilePayloadFormat,
    tile_compression: TileCompression,
    seam_reconcile_layers: &'a [u8],
    seam_metrics: &'a SeamMetrics,
}

#[derive(Clone, Copy)]
struct ArtifactPartitionCtx<'a> {
    ocean: &'a crate::ocean::OceanTiles,
    band_empty: bool,
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
#[hotpath::measure]
pub(super) fn phase_assemble(
    sort_reader: &mut sort::SortReader,
    config: &TilegenConfig,
) -> Result<
    (
        u64,
        u64,
        u64,
        usize,
        pmtiles_writer::DedupStats,
        TileSizeDiagnostics,
    ),
    PipelineError,
> {
    phase_assemble_with_ocean(sort_reader, config, None)
}

#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub(super) fn phase_assemble_with_ocean(
    sort_reader: &mut sort::SortReader,
    config: &TilegenConfig,
    ocean_tiles: Option<std::sync::Arc<crate::ocean::OceanTiles>>,
) -> Result<
    (
        u64,
        u64,
        u64,
        usize,
        pmtiles_writer::DedupStats,
        TileSizeDiagnostics,
    ),
    PipelineError,
> {
    use std::sync::mpsc::sync_channel;

    let pmtiles_config = PmtilesConfig {
        min_zoom: config.min_zoom,
        max_zoom: config.max_zoom,
        bounds: (-180.0, -85.05, 180.0, 85.05),
        center: (0.0, 0.0, 2),
    };
    let mut pmtiles = if config.in_memory {
        PmtilesWriter::new(pmtiles_config)
    } else {
        PmtilesWriter::new_streaming(pmtiles_config, &config.tmp_dir)?
    };
    match config.tile_format {
        TilePayloadFormat::Mvt => {
            let compression = match config.tile_compression {
                TileCompression::Gzip => TileDataCompression::Gzip,
                TileCompression::Brotli => TileDataCompression::Brotli,
            };
            pmtiles.set_tile_contract(TileDataFormat::Mvt, compression);
        }
        TilePayloadFormat::Mlt => {
            pmtiles.set_tile_contract(TileDataFormat::Mlt, TileDataCompression::None);
        }
    }
    if let Some(key) = &config.ocean_artifact_key {
        pmtiles.set_metadata_extension(format!("\"ocean_artifact\":{}", key.json()));
    }
    if config.ocean_only_metadata {
        pmtiles.set_ocean_only_metadata();
    }

    const BATCH_SIZE: usize = 4096;
    let assemble_budget = if config.assemble_batch_budget > 0 {
        config.assemble_batch_budget
    } else {
        32 * 1024 * 1024 // 32 MB default
    };

    // Double-buffer pipeline: reader → encoder (main/rayon) → writer.
    // sync_channel(1) allows one batch ahead, overlapping read/write I/O
    // with CPU-bound rayon encoding.
    // Error cascade: reader error → drops read_tx → encoder loop ends →
    // drops encode_tx → writer loop ends → scope joins → error propagated.
    let (read_tx, read_rx) = sync_channel::<Vec<PendingTile>>(1);
    let (encode_tx, encode_rx) = sync_channel::<Vec<EncodedTile>>(1);

    let seam_metrics = SeamMetrics::new();
    let scope_result: Result<AssembleCore, PipelineError> =
        if let Some(partitions) = sort_reader.take_partitions() {
            phase_assemble_partitions(
                &partitions,
                pmtiles,
                config,
                &seam_metrics,
                ocean_tiles.as_deref(),
            )
        } else {
            std::thread::scope(|s| {
                // --- Reader thread: k-way merge → PendingTile batches ---
                let reader = s.spawn(move || -> Result<(u64, usize, u64), PipelineError> {
                    // Single wall-clock span for the whole thread, not per-record: this
                    // loop calls sort_reader.next() up to ~512M times at NA scale, and
                    // the k-way merge's read_record() is exactly this thread's serial
                    // bottleneck (perf-hunt item 14) - per-call #[hotpath::measure]
                    // would add two clock reads per call, the same overhead problem
                    // node_index.rs's get_from_group_cached explicitly avoids at a
                    // similar call count. One Instant::now() pair gives this thread's
                    // total wall time, comparable against phase_assemble's total to
                    // see how much of assemble is this serial reader.
                    let reader_started = std::time::Instant::now();
                    let mut features_read: u64 = 0;
                    let mut batch: Vec<PendingTile> = Vec::with_capacity(BATCH_SIZE);
                    let mut current = PendingTile {
                        tile_id: u64::MAX,
                        features: Vec::new(),
                    };
                    // Incremental byte tracking for assemble batch HWM.
                    let mut current_tile_bytes: usize = 0;
                    let mut batch_bytes: usize = 0;
                    let mut max_batch_bytes: usize = 0;

                    loop {
                        let record = sort_reader.next()?;
                        let Some(r) = record else {
                            if current.tile_id != u64::MAX {
                                batch_bytes += 32 + current_tile_bytes;
                                batch.push(current);
                            }
                            if !batch.is_empty() {
                                if batch_bytes > max_batch_bytes {
                                    max_batch_bytes = batch_bytes;
                                }
                                let send_result = {
                                    let _wait = wait_span(&WAIT.assemble_reader_backpressure);
                                    read_tx.send(batch)
                                };
                                drop(send_result); // ignore: encoder may have exited
                            }
                            break;
                        };
                        features_read += 1;

                        let tile_id = sort::tile_id_from_key(r.key);
                        let layer_idx = sort::layer_from_key(r.key);

                        if tile_id != current.tile_id {
                            if current.tile_id != u64::MAX {
                                batch_bytes += 32 + current_tile_bytes;
                                batch.push(current);
                                if batch.len() >= BATCH_SIZE || batch_bytes >= assemble_budget {
                                    if batch_bytes > max_batch_bytes {
                                        max_batch_bytes = batch_bytes;
                                    }
                                    let send_result = {
                                        let _wait = wait_span(&WAIT.assemble_reader_backpressure);
                                        read_tx.send(batch)
                                    };
                                    if send_result.is_err() {
                                        break;
                                    }
                                    batch = Vec::with_capacity(BATCH_SIZE);
                                    batch_bytes = 0;
                                }
                            }
                            current = PendingTile {
                                tile_id,
                                features: Vec::new(),
                            };
                            current_tile_bytes = 0;
                        }
                        let data_len = r.data.len();
                        current.features.push((layer_idx, r.data));
                        current_tile_bytes += 32 + data_len;
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    let reader_ns = reader_started.elapsed().as_nanos() as u64;
                    Ok((features_read, max_batch_bytes, reader_ns))
                });

                // --- Writer thread: encoded tiles → PMTiles ---
                // move takes ownership of pmtiles; returned via join handle for write_to().
                let writer = s.spawn(
                move || -> (
                    u64,
                    PmtilesWriter,
                    [u64; 15],
                    [u64; 15],
                    [u64; 15],
                    TileSizeDiagnostics,
                ) {
                    let mut pmtiles = pmtiles;
                    let mut tiles_written: u64 = 0;
                    let mut tiles_per_zoom = [0u64; 15];
                    let mut unique_per_zoom = [0u64; 15];
                    let mut bytes_per_zoom = [0u64; 15];
                    let mut size_diag = TileSizeDiagnostics::default();
                    loop {
                        let batch = {
                            let _wait = wait_span(&WAIT.assemble_write_input);
                            match encode_rx.recv() {
                                Ok(batch) => batch,
                                Err(_) => break,
                            }
                        };
                        for tile in batch {
                            let (z, x, y) = pmtiles_writer::tile_id_to_zxy(tile.tile_id);
                            let tile_bytes = tile.compressed.len() as u64;
                            record_tile_size_diagnostics(&mut size_diag, tile.tile_id, tile_bytes);
                            // Panic: disk I/O failure is unrecoverable mid-pipeline.
                            let is_unique = pmtiles
                                .add_tile(z, x, y, &tile.compressed)
                                .expect("failed to write tile");
                            tiles_written += 1;
                            if (z as usize) < 15 {
                                tiles_per_zoom[z as usize] += 1;
                                if is_unique {
                                    unique_per_zoom[z as usize] += 1;
                                    bytes_per_zoom[z as usize] += tile_bytes;
                                }
                            }
                        }
                    }
                    (
                        tiles_written,
                        pmtiles,
                        tiles_per_zoom,
                        unique_per_zoom,
                        bytes_per_zoom,
                        size_diag,
                    )
                },
            );

                // --- Main thread: receive batches, encode with rayon, forward to writer ---
                let compression_level = config.compression_level;
                let tile_format = config.tile_format;
                let tile_compression = config.tile_compression;
                loop {
                    let batch = {
                        let _wait = wait_span(&WAIT.assemble_encode_input);
                        match read_rx.recv() {
                            Ok(batch) => batch,
                            Err(_) => break,
                        }
                    };
                    let encoded = encode_tile_batch(
                        &batch,
                        compression_level,
                        tile_format,
                        tile_compression,
                        &config.seam_reconcile_layers,
                        &seam_metrics,
                    )?;
                    let send_result = {
                        let _wait = wait_span(&WAIT.assemble_writer_backpressure);
                        encode_tx.send(encoded)
                    };
                    if send_result.is_err() {
                        break;
                    }
                }
                drop(encode_tx);

                let (features_read, max_batch_bytes, reader_ns) = {
                    let _wait = wait_span(&WAIT.assemble_reader_join);
                    reader.join().expect("reader panicked")
                }?;
                let (
                    tiles_written,
                    pmtiles,
                    tiles_per_zoom,
                    unique_per_zoom,
                    bytes_per_zoom,
                    size_diag,
                ) = {
                    let _wait = wait_span(&WAIT.assemble_writer_join);
                    writer.join().expect("writer panicked")
                };
                Ok(AssembleCore {
                    features_read,
                    tiles_written,
                    pmtiles,
                    tiles_per_zoom,
                    unique_per_zoom,
                    bytes_per_zoom,
                    max_batch_bytes,
                    size_diag,
                    reader_ns,
                    reader_total_ns: None,
                    partition_workers: None,
                    partition_count: None,
                })
            })
        };

    let AssembleCore {
        features_read,
        tiles_written,
        mut pmtiles,
        tiles_per_zoom,
        unique_per_zoom,
        bytes_per_zoom,
        max_batch_bytes,
        size_diag,
        reader_ns,
        reader_total_ns,
        partition_workers,
        partition_count,
    } = scope_result?;
    if let (Some(total_ns), Some(workers), Some(partitions)) =
        (reader_total_ns, partition_workers, partition_count)
    {
        eprintln!(
            "  Assemble partition readers: {} partitions, {} workers, max {:.1}s, total {:.1}s",
            partitions,
            workers,
            reader_ns as f64 / 1_000_000_000.0,
            total_ns as f64 / 1_000_000_000.0
        );
        emit_counter_u64("assemble_reader_total_ns", total_ns);
        emit_counter_usize("assemble_partition_workers", workers);
        emit_counter_usize("assemble_partitions", partitions);
    } else {
        eprintln!(
            "  Assemble reader thread (k-way merge): {:.1}s",
            reader_ns as f64 / 1_000_000_000.0
        );
    }
    emit_counter_u64("assemble_reader_ns", reader_ns);
    if let Some(filename) = config.pbf_path.file_name().and_then(|s| s.to_str()) {
        pmtiles.set_source_pbf_filename(filename.to_string());
    } else {
        pmtiles.set_source_pbf_filename(config.pbf_path.display().to_string());
    }
    if let Ok(reader) = ElementReader::from_path(&config.pbf_path)
        && let Some(ts) = reader.header().osmosis_replication_timestamp()
    {
        pmtiles.set_osmosis_replication_timestamp(ts);
    }
    let unique_tiles = pmtiles.unique_tile_count();
    let dedup_stats = pmtiles.dedup_stats().clone();
    {
        let _wait = wait_span(&WAIT.pmtiles_write);
        pmtiles.write_to(&config.output_path)?;
    }

    // Per-zoom tile breakdown
    eprintln!("  Per-zoom tiles (total / unique / unique MB):");
    for z in config.min_zoom..=config.max_zoom {
        let total = tiles_per_zoom[z as usize];
        let unique = unique_per_zoom[z as usize];
        let mb = bytes_per_zoom[z as usize] as f64 / (1024.0 * 1024.0);
        if total > 0 {
            eprintln!("    z{z:2}: {total:>8} / {unique:>8} / {mb:>7.1} MB");
        }
    }

    // Shared-edge reconciliation metrics.
    let seam_touched = seam_metrics.tiles_touched.load(Ordering::Relaxed);
    if seam_touched > 0 {
        let seam_layer_descs: Vec<String> = config
            .seam_reconcile_layers
            .iter()
            .enumerate()
            .filter(|(_, max_z)| **max_z > 0)
            .map(|(i, max_z)| format!("{}:z{}", shortbread::Layer::ALL[i].name(), max_z))
            .collect();
        let seam_rings = seam_metrics.rings_decoded.load(Ordering::Relaxed);
        let seam_chains = seam_metrics.chains_detected.load(Ordering::Relaxed);
        let seam_reconciled = seam_metrics.chains_reconciled.load(Ordering::Relaxed);
        let seam_skipped = seam_metrics.chains_skipped.load(Ordering::Relaxed);
        let seam_us = seam_metrics.reconcile_us.load(Ordering::Relaxed);
        eprintln!(
            "  Seam reconciliation ({}): {} tiles, {} rings, {} chains ({} reconciled, {} skipped), {:.1} ms",
            seam_layer_descs.join("+"),
            seam_touched,
            seam_rings,
            seam_chains,
            seam_reconciled,
            seam_skipped,
            seam_us as f64 / 1000.0,
        );
        emit_counter_u64("seam_tiles_touched", seam_touched);
        emit_counter_u64("seam_rings_decoded", seam_rings);
        emit_counter_u64("seam_chains_detected", seam_chains);
        emit_counter_u64("seam_chains_reconciled", seam_reconciled);
        emit_counter_u64("seam_chains_skipped", seam_skipped);
        emit_counter_u64("seam_reconcile_us", seam_us);
    }

    Ok((
        features_read,
        tiles_written,
        unique_tiles,
        max_batch_bytes,
        dedup_stats,
        size_diag,
    ))
}

#[allow(clippy::too_many_lines)]
fn phase_assemble_partitions(
    partitions: &[sort::SortPartition],
    mut pmtiles: PmtilesWriter,
    config: &TilegenConfig,
    seam_metrics: &SeamMetrics,
    ocean: Option<&crate::ocean::OceanTiles>,
) -> Result<AssembleCore, PipelineError> {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::mpsc;

    let union = partition_union(partitions, ocean);
    let partition_count = union.len();
    if partition_count == 0 {
        return Ok(AssembleCore {
            features_read: 0,
            tiles_written: 0,
            pmtiles,
            tiles_per_zoom: [0; 15],
            unique_per_zoom: [0; 15],
            bytes_per_zoom: [0; 15],
            max_batch_bytes: 0,
            size_diag: TileSizeDiagnostics::default(),
            reader_ns: 0,
            reader_total_ns: Some(0),
            partition_workers: Some(0),
            partition_count: Some(0),
        });
    }

    // Measured on NA locations (2026-07-09): 4 workers left the writer idle
    // 68% of assemble; 8 workers cut assemble 186.4 -> 173.0s under the old
    // count window and 160.7s with the byte-budgeted claim window; 12 was
    // WORSE (176.1s, +3 GB RSS) - encode CPU saturates around 8. Env
    // override for future A/Bs.
    let worker_cap = std::env::var("ELIVAGAR_ASSEMBLE_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(8);
    let worker_count = config.threads.clamp(1, worker_cap).min(partition_count);
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
    let park_budget = std::env::var("ELIVAGAR_ASSEMBLE_PARK_BUDGET")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(2 * 1024 * 1024 * 1024);
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
    let compression_level = config.compression_level;
    let tile_format = config.tile_format;
    let tile_compression = config.tile_compression;
    let seam_reconcile_layers = config.seam_reconcile_layers;
    let assemble_budget = if config.assemble_batch_budget > 0 {
        config.assemble_batch_budget
    } else {
        32 * 1024 * 1024
    };
    let artifact_ctx = ocean.map(|ocean| ArtifactPartitionCtx {
        ocean,
        band_empty: ocean
            .grids()
            .iter()
            .all(|grid| grid.band_is_empty(config.min_zoom, config.max_zoom)),
    });

    let mut features_read: u64 = 0;
    let mut tiles_written: u64 = 0;
    let mut tiles_per_zoom = [0u64; 15];
    let mut unique_per_zoom = [0u64; 15];
    let mut bytes_per_zoom = [0u64; 15];
    let mut max_batch_bytes: usize = 0;
    let mut size_diag = TileSizeDiagnostics::default();
    let mut reader_max_ns: u64 = 0;
    let mut reader_total_ns: u64 = 0;
    let mut max_parked_bytes: usize = 0;
    let mut current_partition_encoded: usize = 0;
    let mut max_partition_encoded: usize = 0;

    let scope_result: Result<(), PipelineError> = std::thread::scope(|s| {
        for _ in 0..worker_count {
            let tx = tx.clone();
            let union_ref = &union;
            let next_job_ref = &next_job;
            let stop_ref = &stop;
            let seam_layers = seam_reconcile_layers;
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
                    let result = match union_ref[order] {
                        UnionPartition::Sort(partition) => read_encode_partition(
                            order,
                            partition,
                            compression,
                            compression_level,
                            tile_format,
                            tile_compression,
                            &seam_layers,
                            seam_metrics,
                            assemble_budget,
                            artifact_ctx,
                            &tx,
                        ),
                        UnionPartition::ArtifactOnly { start, end } => artifact_ctx
                            .ok_or_else(|| {
                                PipelineError(
                                    "artifact-only partition without an ocean artifact".to_string(),
                                )
                            })
                            .and_then(|artifact| {
                                let items = artifact_run_copy_items(
                                    artifact.ocean,
                                    start,
                                    end,
                                    artifact.band_empty,
                                )?;
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
                            PartitionItem::Encoded(tile) => write_encoded_item(
                                &mut pmtiles,
                                &tile,
                                &mut tiles_written,
                                &mut tiles_per_zoom,
                                &mut unique_per_zoom,
                                &mut bytes_per_zoom,
                                &mut size_diag,
                            )?,
                            PartitionItem::RunCopy {
                                tile_id,
                                run_length,
                                offset,
                                length,
                            } => {
                                let ocean = ocean.ok_or_else(|| {
                                    PipelineError(
                                        "artifact run copy without an ocean artifact".to_string(),
                                    )
                                })?;
                                write_run_copy(
                                    &mut pmtiles,
                                    tile_id,
                                    run_length,
                                    ocean.raw_blob(offset, length)?,
                                    &mut tiles_written,
                                    &mut tiles_per_zoom,
                                    &mut unique_per_zoom,
                                    &mut bytes_per_zoom,
                                    &mut size_diag,
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
        tiles_written,
        pmtiles,
        tiles_per_zoom,
        unique_per_zoom,
        bytes_per_zoom,
        max_batch_bytes,
        size_diag,
        reader_ns: reader_max_ns,
        reader_total_ns: Some(reader_total_ns),
        partition_workers: Some(worker_count),
        partition_count: Some(partition_count),
    })
}

#[allow(clippy::too_many_arguments)]
fn read_encode_partition(
    order: usize,
    partition: &sort::SortPartition,
    compression: sort::ChunkCompression,
    compression_level: u32,
    tile_format: TilePayloadFormat,
    tile_compression: TileCompression,
    seam_reconcile_layers: &[u8],
    seam_metrics: &SeamMetrics,
    assemble_budget: usize,
    artifact: Option<ArtifactPartitionCtx<'_>>,
    tx: &std::sync::mpsc::Sender<Result<PartitionBatch, PipelineError>>,
) -> Result<(), PipelineError> {
    const BATCH_SIZE: usize = 4096;

    let mut reader = sort::SortPartitionReader::open(partition, compression)?;
    let mut batch_features_read: u64 = 0;
    let mut batch: Vec<PendingTile> = Vec::with_capacity(BATCH_SIZE);
    let mut current = PendingTile {
        tile_id: u64::MAX,
        features: Vec::new(),
    };
    let mut current_tile_bytes: usize = 0;
    let mut batch_bytes: usize = 0;
    let mut batch_index = 0usize;
    let mut read_started = std::time::Instant::now();
    let (partition_start, partition_end) = sort::partition_tile_range(partition.index);
    let mut artifact_cursor = partition_start;
    let encode_ctx = PartitionEncodeCtx {
        compression_level,
        tile_format,
        tile_compression,
        seam_reconcile_layers,
        seam_metrics,
    };

    loop {
        let record = reader.next()?;
        let Some(r) = record else {
            if current.tile_id != u64::MAX {
                batch_bytes += 32 + current_tile_bytes;
                batch.push(current);
            }
            let mut reader_ns = 0;
            add_reader_elapsed(&mut reader_ns, read_started);
            encode_and_send_partition_batch(
                tx,
                &encode_ctx,
                &PartitionBatchMeta {
                    order,
                    batch_index,
                    is_last: true,
                    features_read: batch_features_read,
                    max_batch_bytes: if batch.is_empty() { 0 } else { batch_bytes },
                    reader_ns,
                },
                &batch,
                artifact,
                &mut artifact_cursor,
                Some(partition_end),
            )?;
            break;
        };
        batch_features_read += 1;

        let tile_id = sort::tile_id_from_key(r.key);
        let layer_idx = sort::layer_from_key(r.key);
        if tile_id != current.tile_id {
            if current.tile_id != u64::MAX {
                batch_bytes += 32 + current_tile_bytes;
                batch.push(current);
                if batch.len() >= BATCH_SIZE || batch_bytes >= assemble_budget {
                    let mut reader_ns = 0;
                    add_reader_elapsed(&mut reader_ns, read_started);
                    encode_and_send_partition_batch(
                        tx,
                        &encode_ctx,
                        &PartitionBatchMeta {
                            order,
                            batch_index,
                            is_last: false,
                            features_read: batch_features_read,
                            max_batch_bytes: batch_bytes,
                            reader_ns,
                        },
                        &batch,
                        artifact,
                        &mut artifact_cursor,
                        None,
                    )?;
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
        current.features.push((layer_idx, r.data));
        current_tile_bytes += 32 + data_len;
    }

    Ok(())
}

fn artifact_run_copy_items(
    ocean: &crate::ocean::OceanTiles,
    start: u64,
    end: u64,
    band_empty: bool,
) -> Result<Vec<PartitionItem>, PipelineError> {
    let mut items = Vec::new();
    if start >= end {
        return Ok(items);
    }
    for run in ocean.runs_in(start, end) {
        let mut cursor = start.max(run.tile_id);
        let run_end = end.min(run.tile_id.saturating_add(u64::from(run.run_length)));
        if !band_empty {
            // A computed band suppresses the artifact even when it emits no
            // tile. Coalesce the remaining members, while retaining zoom
            // boundaries for the per-zoom writer counters.
            let mut copy_start = None;
            let mut copy_zoom = None;
            while cursor < run_end {
                let (z, x, y) = pmtiles_writer::tile_id_to_zxy(cursor);
                let copy = !ocean.band_tile(z, x, y);
                if copy && copy_zoom.is_none_or(|previous| previous == z) {
                    copy_start.get_or_insert(cursor);
                    copy_zoom = Some(z);
                } else {
                    if let Some(start) = copy_start.take() {
                        items.push(PartitionItem::RunCopy {
                            tile_id: start,
                            run_length: u32::try_from(cursor - start).map_err(|_| {
                                PipelineError("artifact split run length exceeds u32".to_string())
                            })?,
                            offset: run.offset,
                            length: run.length,
                        });
                    }
                    copy_zoom = None;
                    if copy {
                        copy_start = Some(cursor);
                        copy_zoom = Some(z);
                    }
                }
                cursor += 1;
            }
            if let Some(start) = copy_start {
                items.push(PartitionItem::RunCopy {
                    tile_id: start,
                    run_length: u32::try_from(run_end - start).map_err(|_| {
                        PipelineError("artifact split run length exceeds u32".to_string())
                    })?,
                    offset: run.offset,
                    length: run.length,
                });
            }
            continue;
        }
        while cursor < run_end {
            let (z, _x, _y) = pmtiles_writer::tile_id_to_zxy(cursor);
            let zoom_end = if z == 14 {
                run_end
            } else {
                ((1_u64 << (2 * u32::from(z + 1))) - 1) / 3
            };
            let sub_end = run_end.min(zoom_end);
            let length = u32::try_from(sub_end - cursor)
                .map_err(|_| PipelineError("artifact split run length exceeds u32".to_string()))?;
            items.push(PartitionItem::RunCopy {
                tile_id: cursor,
                run_length: length,
                offset: run.offset,
                length: run.length,
            });
            cursor = sub_end;
        }
    }
    Ok(items)
}

#[allow(clippy::too_many_arguments)]
fn write_run_copy(
    pmtiles: &mut PmtilesWriter,
    tile_id: u64,
    run_length: u32,
    data: &[u8],
    tiles_written: &mut u64,
    tiles_per_zoom: &mut [u64; 15],
    unique_per_zoom: &mut [u64; 15],
    bytes_per_zoom: &mut [u64; 15],
    size_diag: &mut TileSizeDiagnostics,
) -> Result<(), PipelineError> {
    let (z, _, _) = pmtiles_writer::tile_id_to_zxy(tile_id);
    let bytes = u64::try_from(data.len())
        .map_err(|_| PipelineError("tile payload length exceeds u64".to_string()))?;
    let unique = pmtiles.add_run(tile_id, run_length, data)?;
    *tiles_written += u64::from(run_length);
    tiles_per_zoom[z as usize] += u64::from(run_length);
    if unique {
        unique_per_zoom[z as usize] += 1;
        bytes_per_zoom[z as usize] += bytes;
    }
    record_tile_size_diagnostics(size_diag, tile_id, bytes);
    if run_length > 1 {
        size_diag.total_tile_bytes += bytes * u64::from(run_length - 1);
        if bytes > super::stats::TILE_OVERSIZE_WARN_BYTES {
            size_diag.oversize_warn_count += u64::from(run_length - 1);
        }
        if bytes > super::stats::TILE_OVERSIZE_SEVERE_BYTES {
            size_diag.oversize_severe_count += u64::from(run_length - 1);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_encoded_item(
    pmtiles: &mut PmtilesWriter,
    tile: &EncodedTile,
    tiles_written: &mut u64,
    tiles_per_zoom: &mut [u64; 15],
    unique_per_zoom: &mut [u64; 15],
    bytes_per_zoom: &mut [u64; 15],
    size_diag: &mut TileSizeDiagnostics,
) -> Result<(), PipelineError> {
    let (z, x, y) = pmtiles_writer::tile_id_to_zxy(tile.tile_id);
    let bytes = tile.compressed.len() as u64;
    record_tile_size_diagnostics(size_diag, tile.tile_id, bytes);
    let unique = pmtiles.add_tile(z, x, y, &tile.compressed)?;
    *tiles_written += 1;
    tiles_per_zoom[z as usize] += 1;
    if unique {
        unique_per_zoom[z as usize] += 1;
        bytes_per_zoom[z as usize] += bytes;
    }
    Ok(())
}

fn raw_contains_ocean(raw: &[u8]) -> Result<bool, PipelineError> {
    Ok(crate::pmtiles_reader::decode_mvt_layers(raw)?
        .iter()
        .any(|layer| layer.name == "ocean"))
}

fn ocean_layer_field(compressed: &[u8]) -> Result<Vec<u8>, PipelineError> {
    let raw = crate::pmtiles_reader::gzip_decompress(compressed)?;
    let layers = crate::pmtiles_reader::decode_mvt_layers(&raw)?;
    if layers.len() != 1 || layers[0].name != "ocean" {
        return Err(PipelineError(
            "artifact tile must contain exactly one ocean layer".to_string(),
        ));
    }
    let mut i = 0;
    while i < raw.len() {
        let begin = i;
        let tag = read_varint(&raw, &mut i)?;
        let field = tag >> 3;
        match tag & 7 {
            0 => {
                let _ = read_varint(&raw, &mut i)?;
            }
            1 => {
                i = i
                    .checked_add(8)
                    .filter(|end| *end <= raw.len())
                    .ok_or_else(|| {
                        PipelineError("MVT fixed64 field exceeds payload".to_string())
                    })?;
            }
            2 => {
                let length = usize::try_from(read_varint(&raw, &mut i)?)
                    .map_err(|_| PipelineError("MVT field length too large".to_string()))?;
                let end = i
                    .checked_add(length)
                    .filter(|end| *end <= raw.len())
                    .ok_or_else(|| PipelineError("MVT field exceeds payload".to_string()))?;
                if field == 3 {
                    return Ok(raw[begin..end].to_vec());
                }
                i = end;
            }
            5 => {
                i = i
                    .checked_add(4)
                    .filter(|end| *end <= raw.len())
                    .ok_or_else(|| {
                        PipelineError("MVT fixed32 field exceeds payload".to_string())
                    })?;
            }
            _ => return Err(PipelineError("unsupported MVT field wire type".to_string())),
        }
    }
    Err(PipelineError(
        "artifact ocean tile missing layer field".to_string(),
    ))
}

fn read_varint(data: &[u8], cursor: &mut usize) -> Result<u64, PipelineError> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = *data
            .get(*cursor)
            .ok_or_else(|| PipelineError("truncated MVT varint".to_string()))?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(PipelineError("overlong MVT varint".to_string()))
}

fn splice_ocean_layer(
    mut raw: Vec<u8>,
    field: &[u8],
    tile_id: u64,
    compression_level: u32,
) -> Result<Vec<u8>, PipelineError> {
    raw.extend_from_slice(field);
    let (z, _, _) = pmtiles_writer::tile_id_to_zxy(tile_id);
    let level = compression_level_for_zoom(z, compression_level);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
    use std::io::Write;
    encoder.write_all(&raw)?;
    Ok(encoder.finish()?)
}

/// Match the compression regime used by normal MVT batch encoding.
fn compression_level_for_zoom(z: u8, compression_level: u32) -> u32 {
    match z {
        0..=8 => compression_level.clamp(9, 10),
        13..=14 => compression_level.min(3),
        _ => compression_level,
    }
}

fn encode_and_send_partition_batch(
    tx: &std::sync::mpsc::Sender<Result<PartitionBatch, PipelineError>>,
    ctx: &PartitionEncodeCtx<'_>,
    meta: &PartitionBatchMeta,
    pending_tiles: &[PendingTile],
    artifact: Option<ArtifactPartitionCtx<'_>>,
    artifact_cursor: &mut u64,
    final_end: Option<u64>,
) -> Result<(), PipelineError> {
    let encoded_tiles = if pending_tiles.is_empty() {
        Vec::new()
    } else {
        encode_tile_batch(
            pending_tiles,
            ctx.compression_level,
            ctx.tile_format,
            ctx.tile_compression,
            ctx.seam_reconcile_layers,
            ctx.seam_metrics,
        )?
    };
    let mut items = Vec::with_capacity(encoded_tiles.len());
    if let Some(artifact) = artifact {
        let mut encoded = encoded_tiles.into_iter().peekable();
        for tile in pending_tiles {
            items.extend(artifact_run_copy_items(
                artifact.ocean,
                *artifact_cursor,
                tile.tile_id,
                artifact.band_empty,
            )?);
            if encoded
                .peek()
                .is_some_and(|encoded| encoded.tile_id == tile.tile_id)
            {
                let Some(mut encoded) = encoded.next() else {
                    return Err(PipelineError(
                        "peeked encoded tile disappeared from partition batch".to_string(),
                    ));
                };
                splice_artifact_ocean(&mut encoded, artifact, ctx.compression_level)?;
                items.push(PartitionItem::Encoded(encoded));
            }
            *artifact_cursor = tile.tile_id.saturating_add(1);
        }
        if encoded.next().is_some() {
            return Err(PipelineError(
                "encoded tile did not correspond to a partition input tile".to_string(),
            ));
        }
        if let Some(end) = final_end {
            items.extend(artifact_run_copy_items(
                artifact.ocean,
                *artifact_cursor,
                end,
                artifact.band_empty,
            )?);
        }
    } else {
        items.extend(encoded_tiles.into_iter().map(PartitionItem::Encoded));
    }
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

fn splice_artifact_ocean(
    encoded: &mut EncodedTile,
    artifact: ArtifactPartitionCtx<'_>,
    compression_level: u32,
) -> Result<(), PipelineError> {
    let (z, x, y) = pmtiles_writer::tile_id_to_zxy(encoded.tile_id);
    let band = !artifact.band_empty && artifact.ocean.band_tile(z, x, y);
    if band {
        return Ok(());
    }
    let Some(run) = artifact.ocean.run_covering(encoded.tile_id) else {
        return Ok(());
    };
    let raw = crate::pmtiles_reader::gzip_decompress(&encoded.compressed)?;
    if raw_contains_ocean(&raw)? {
        return Err(PipelineError(
            "non-band OSM tile unexpectedly contains computed ocean".to_string(),
        ));
    }
    let ocean_field = ocean_layer_field(artifact.ocean.raw_blob(run.offset, run.length)?)?;
    encoded.compressed = splice_ocean_layer(raw, &ocean_field, encoded.tile_id, compression_level)?;
    Ok(())
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

/// Per-worker assembly state, persisted across batches via `thread_local!`.
/// Avoids re-creating Compressor + pools on every batch boundary and keeps
/// LayerBuilder HashMap capacity alive across tiles.
pub(super) struct AssemblyScratch {
    pub(super) encode_scratch: mvt::EncodeScratch,
    pub(super) merge_scratch: mvt::MergeScratch,
    pub(super) line_merge_scratch: mvt::LineMergeScratch,
    pub(super) geom_pool: Vec<Vec<u32>>,
    pub(super) tags_pool: Vec<Vec<(u16, u16)>>,
    pub(super) compression_levels: [Option<flate2::Compression>; 11],
    pub(super) gz_buf: Vec<u8>,
    pub(super) mvt_buf: Vec<u8>,
    pub(super) layers: [Option<LayerBuilder>; LAYER_COUNT],
    pub(super) seam_rings: Vec<Vec<(i32, i32)>>,
    pub(super) seam_provenance: Vec<(usize, usize)>,
    pub(super) seam_encode_buf: Vec<u32>,
}

/// Metrics for shared-edge reconciliation in the assemble phase.
pub(super) struct SeamMetrics {
    pub(super) tiles_touched: AtomicU64,
    pub(super) rings_decoded: AtomicU64,
    pub(super) chains_detected: AtomicU64,
    pub(super) chains_reconciled: AtomicU64,
    pub(super) chains_skipped: AtomicU64,
    pub(super) reconcile_us: AtomicU64,
}

impl SeamMetrics {
    pub(super) fn new() -> Self {
        Self {
            tiles_touched: AtomicU64::new(0),
            rings_decoded: AtomicU64::new(0),
            chains_detected: AtomicU64::new(0),
            chains_reconciled: AtomicU64::new(0),
            chains_skipped: AtomicU64::new(0),
            reconcile_us: AtomicU64::new(0),
        }
    }
}

impl AssemblyScratch {
    /// Built fresh per tile, NOT pooled per thread. The pooled version grew
    /// every buffer to the fattest tile each rayon thread ever saw - geom
    /// pool inner capacities, LayerBuilder feature/interning storage, merge
    /// scratch - summing to a measured ~17 GB live plateau on NA locations
    /// assemble (identical under mimalloc/glibc/jemalloc: live, not
    /// retention). Rebuilding per tile measured RSS 7.2 -> 1.8 GB on germany
    /// assemble-only with wall UNCHANGED (21.6s both ways) - the reuse
    /// bought nothing the encode work didn't dwarf.
    fn new() -> Self {
        AssemblyScratch {
            encode_scratch: mvt::EncodeScratch::new(),
            merge_scratch: mvt::MergeScratch::new(),
            line_merge_scratch: mvt::LineMergeScratch::new(),
            geom_pool: Vec::new(),
            tags_pool: Vec::new(),
            compression_levels: [const { None }; 11],
            gz_buf: Vec::new(),
            mvt_buf: Vec::new(),
            layers: [const { None }; LAYER_COUNT],
            seam_rings: Vec::new(),
            seam_provenance: Vec::new(),
            seam_encode_buf: Vec::new(),
        }
    }
}

/// Shared-edge reconciliation for boundary polygon features in a single tile.
///
/// Decodes polygon MVT commands → tile-coord rings, detects shared chains,
/// copies canonical vertex sequences to matching rings, simplifies non-shared
/// segments with tile-coordinate DP, and re-encodes back to MVT commands.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn reconcile_boundary_seams(
    lb: &mut LayerBuilder,
    seam_rings: &mut Vec<Vec<(i32, i32)>>,
    seam_provenance: &mut Vec<(usize, usize)>,
    encode_buf: &mut Vec<u32>,
    metrics: &SeamMetrics,
) {
    let start = std::time::Instant::now();

    // Collect all polygon rings from features in this layer.
    seam_rings.clear();
    seam_provenance.clear();

    let features = lb.features_mut();
    let mut polygon_feature_indices: Vec<usize> = Vec::new();

    for (fi, feat) in features.iter().enumerate() {
        if feat.geom_type != GeomType::Polygon {
            continue;
        }
        let decoded = geometry::decode_mvt_polygon(&feat.geometry);
        let valid_count = decoded
            .iter()
            .filter(|r| r.len() >= 4 && r.first() == r.last())
            .count();
        if valid_count == 0 {
            continue;
        }
        let ring_start = seam_rings.len();
        for ring in decoded {
            if ring.len() >= 4 && ring.first() == ring.last() {
                seam_rings.push(ring);
            }
        }
        let ring_count = seam_rings.len() - ring_start;
        polygon_feature_indices.push(fi);
        seam_provenance.push((fi, ring_count));
    }

    if seam_rings.is_empty() {
        let elapsed_us = start.elapsed().as_micros() as u64;
        metrics
            .reconcile_us
            .fetch_add(elapsed_us, Ordering::Relaxed);
        return;
    }

    metrics.tiles_touched.fetch_add(1, Ordering::Relaxed);
    metrics
        .rings_decoded
        .fetch_add(seam_rings.len() as u64, Ordering::Relaxed);

    // Detect shared chains (needs >= 2 rings to find any).
    let chains = if seam_rings.len() >= 2 {
        geometry::detect_shared_chains(seam_rings)
    } else {
        Vec::new()
    };
    metrics
        .chains_detected
        .fetch_add(chains.len() as u64, Ordering::Relaxed);

    // Canonicalize: copy first incident's vertices to second incident's ring.
    if !chains.is_empty() {
        let canon_result = geometry::canonicalize_shared_chains(seam_rings, &chains);
        metrics
            .chains_reconciled
            .fetch_add(canon_result.reconciled as u64, Ordering::Relaxed);
        metrics
            .chains_skipped
            .fetch_add(canon_result.skipped as u64, Ordering::Relaxed);
    }

    // Tile-coordinate DP on all rings, pinning shared-chain vertices.
    // Runs even when no chains were found - these rings skipped PBF-phase DP
    // and need tile-coord simplification regardless.
    for (ring_idx, ring) in seam_rings.iter_mut().enumerate() {
        let pinned = geometry::build_pinned_mask(ring.len(), ring_idx, &chains);
        let simplified =
            geometry::simplify_ring_tile_coords(ring, &pinned, geometry::TILE_SIMPLIFY_TOLERANCE);
        *ring = simplified;
    }

    // Re-encode each feature's rings back to MVT commands.
    let mut ring_cursor: usize = 0;
    for &(fi, ring_count) in seam_provenance.iter() {
        let feature_rings = &seam_rings[ring_cursor..ring_cursor + ring_count];
        ring_cursor += ring_count;
        let ring_refs: Vec<&[(i32, i32)]> = feature_rings.iter().map(Vec::as_slice).collect();
        mvt::encode_polygon(encode_buf, &ring_refs);
        features[fi].geometry.clear();
        features[fi].geometry.extend_from_slice(encode_buf);
    }

    let elapsed_us = start.elapsed().as_micros() as u64;
    metrics
        .reconcile_us
        .fetch_add(elapsed_us, Ordering::Relaxed);
}

/// Encode + compress a batch of tiles in parallel using rayon.
#[hotpath::measure]
#[allow(clippy::cast_possible_wrap)]
pub(super) fn encode_tile_batch(
    batch: &[PendingTile],
    compression_level: u32,
    tile_format: TilePayloadFormat,
    tile_compression: TileCompression,
    seam_reconcile_layers: &[u8],
    seam_metrics: &SeamMetrics,
) -> Result<Vec<EncodedTile>, PipelineError> {
    match tile_format {
        TilePayloadFormat::Mvt => Ok(encode_tile_batch_mvt(
            batch,
            compression_level,
            tile_compression,
            seam_reconcile_layers,
            seam_metrics,
        )),
        TilePayloadFormat::Mlt => encode_tile_batch_mlt(batch),
    }
}

/// Encode + compress a batch of MVT tiles in parallel using rayon.
#[hotpath::measure]
#[allow(clippy::cast_possible_wrap)]
// Visible only without the hotpath feature (the measure macro's wrapping
// masks it under --all-features).
#[allow(clippy::too_many_lines)]
pub(super) fn encode_tile_batch_mvt(
    batch: &[PendingTile],
    compression_level: u32,
    tile_compression: TileCompression,
    seam_reconcile_layers: &[u8],
    seam_metrics: &SeamMetrics,
) -> Vec<EncodedTile> {
    use rayon::prelude::*;

    batch
        .par_iter()
        .map(|tile| {
            {
                let mut scratch = AssemblyScratch::new();
                let s = &mut scratch;

                for &(layer_idx, ref data) in &tile.features {
                    if (layer_idx as usize) < s.layers.len() {
                        add_feature_to_layer(
                            get_or_create_layer(&mut s.layers, layer_idx as usize),
                            data,
                            &mut s.geom_pool,
                            &mut s.tags_pool,
                        );
                    }
                }

                let (z, _, _) = pmtiles_writer::tile_id_to_zxy(tile.tile_id);

                // Shared-edge reconciliation for polygon layers at low zoom.
                // Must run BEFORE merge_same_attr_geometries (which destroys per-ring identity).
                {
                    for (li, &max_z) in seam_reconcile_layers.iter().enumerate() {
                        if max_z > 0
                            && z <= max_z
                            && let Some(lb) = s.layers[li].as_mut()
                        {
                            reconcile_boundary_seams(
                                lb,
                                &mut s.seam_rings,
                                &mut s.seam_provenance,
                                &mut s.seam_encode_buf,
                                seam_metrics,
                            );
                        }
                    }
                }

                for (li, layer) in s.layers.iter_mut().enumerate() {
                    if li == shortbread::Layer::Ocean as usize {
                        continue;
                    }
                    if let Some(lb) = layer.as_mut() {
                        lb.merge_same_attr_geometries(
                            &mut s.merge_scratch,
                            &mut s.geom_pool,
                            &mut s.tags_pool,
                        );
                    }
                }

                if z < 14 {
                    for layer in &mut s.layers {
                        if let Some(lb) = layer.as_mut() {
                            lb.merge_connected_lines(&mut s.line_merge_scratch);
                        }
                    }
                }

                // Max 26 elements (one per Shortbread layer) - with_capacity not needed.
                let non_empty: Vec<&LayerBuilder> = s
                    .layers
                    .iter()
                    .filter_map(|l| l.as_ref())
                    .filter(|l| !l.is_empty())
                    .collect();
                if non_empty.is_empty() {
                    return None;
                }

                mvt::encode_tile_into(&mut s.mvt_buf, &non_empty, &mut s.encode_scratch);

                if s.mvt_buf.is_empty() {
                    return None;
                }

                // Per-zoom compression: boost low zooms, speed up high zooms.
                let level = usize::try_from(compression_level_for_zoom(z, compression_level))
                    .expect("compression level fits usize");

                let mut compress_buf = std::mem::take(&mut s.gz_buf);
                compress_buf.clear();

                let compressed = match tile_compression {
                    TileCompression::Gzip => {
                        #[allow(clippy::cast_possible_truncation)]
                        let lvl = *s.compression_levels[level]
                            .get_or_insert_with(|| flate2::Compression::new(level as u32));
                        let mut encoder = flate2::write::GzEncoder::new(compress_buf, lvl);
                        std::io::Write::write_all(&mut encoder, &s.mvt_buf)
                            .expect("gzip compress failed");
                        encoder.finish().expect("gzip finish failed")
                    }
                    TileCompression::Brotli => {
                        #[allow(clippy::cast_possible_truncation)]
                        let quality = level as u32;
                        let mut encoder =
                            brotli::CompressorWriter::new(&mut compress_buf, 4096, quality, 22);
                        std::io::Write::write_all(&mut encoder, &s.mvt_buf)
                            .expect("brotli compress failed");
                        drop(encoder);
                        compress_buf
                    }
                };
                Some(EncodedTile {
                    tile_id: tile.tile_id,
                    compressed,
                })
            }
        })
        .flatten()
        .collect()
}

/// Build non-empty per-layer builders for one tile.
pub(super) fn prepare_non_empty_layers<'a>(
    s: &'a mut AssemblyScratch,
    tile: &PendingTile,
) -> Vec<&'a LayerBuilder> {
    // Reset persisted layers from previous tile (reclaim features + clear interning).
    for slot in &mut s.layers {
        if let Some(lb) = slot.as_mut() {
            lb.prepare_for_reuse(&mut s.geom_pool, &mut s.tags_pool);
        }
    }

    for &(layer_idx, ref data) in &tile.features {
        if (layer_idx as usize) < s.layers.len() {
            add_feature_to_layer(
                get_or_create_layer(&mut s.layers, layer_idx as usize),
                data,
                &mut s.geom_pool,
                &mut s.tags_pool,
            );
        }
    }

    for layer in &mut s.layers {
        if let Some(lb) = layer.as_mut() {
            lb.merge_same_attr_geometries(&mut s.merge_scratch, &mut s.geom_pool, &mut s.tags_pool);
        }
    }

    // Max 26 elements (one per Shortbread layer) - with_capacity not needed.
    s.layers
        .iter()
        .filter_map(|l| l.as_ref())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Encode an MLT batch (currently scaffolded, returns not-implemented error with tile context).
#[hotpath::measure]
pub(super) fn encode_tile_batch_mlt(
    batch: &[PendingTile],
) -> Result<Vec<EncodedTile>, PipelineError> {
    use rayon::prelude::*;

    let results: Vec<Result<Option<EncodedTile>, PipelineError>> = batch
        .par_iter()
        .map(|tile| {
            {
                let mut scratch = AssemblyScratch::new();
                let s = &mut scratch;
                let non_empty = prepare_non_empty_layers(s, tile);
                if non_empty.is_empty() {
                    return Ok(None);
                }
                let encoded = mlt::encode_tile(&non_empty).map_err(|err| {
                    let (z, x, y) = pmtiles_writer::tile_id_to_zxy(tile.tile_id);
                    PipelineError(format!("mlt encode failed for tile {z}/{x}/{y}: {err}"))
                })?;
                Ok(Some(EncodedTile {
                    tile_id: tile.tile_id,
                    compressed: encoded, // MLT path currently uses no per-tile compression.
                }))
            }
        })
        .collect();

    let mut out = Vec::new();
    for item in results {
        if let Some(tile) = item? {
            out.push(tile);
        }
    }
    Ok(out)
}

pub(super) const LAYER_COUNT: usize = Layer::count();

/// Get or create a LayerBuilder at the given index.
fn get_or_create_layer(layers: &mut [Option<LayerBuilder>], idx: usize) -> &mut LayerBuilder {
    if layers[idx].is_none() {
        layers[idx] = Some(LayerBuilder::new(Layer::ALL[idx].name()));
    }
    layers[idx].as_mut().expect("just inserted")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn ocean_layer_field_skips_non_length_delimited_tile_fields() {
        // Unknown varint and fixed32 fields precede the MVT layer field. The
        // extractor must skip by wire type, rather than treating every field
        // as length-delimited.
        let layer = [10, 5, b'o', b'c', b'e', b'a', b'n'];
        let raw = [
            8, 1, 21, 0, 0, 0, 0, 26, 7, 10, 5, b'o', b'c', b'e', b'a', b'n',
        ];
        assert_eq!(&raw[9..], layer);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&raw).expect("gzip raw tile");
        let compressed = encoder.finish().expect("finish gzip");
        assert_eq!(
            ocean_layer_field(&compressed).expect("extract layer"),
            raw[7..]
        );
    }
}
