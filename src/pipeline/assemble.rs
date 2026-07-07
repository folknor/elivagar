use std::sync::atomic::{AtomicU64, Ordering};

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
    encoded_tiles: Vec<EncodedTile>,
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

#[derive(Default)]
struct PendingPartitionBatches {
    batches: std::collections::BTreeMap<usize, PartitionBatch>,
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
            phase_assemble_partitions(&partitions, pmtiles, config, &seam_metrics)
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
                                drop(read_tx.send(batch)); // ignore: encoder may have exited
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
                                    if read_tx.send(batch).is_err() {
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
                    while let Ok(batch) = encode_rx.recv() {
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
                for batch in read_rx {
                    let encoded = encode_tile_batch(
                        &batch,
                        compression_level,
                        tile_format,
                        tile_compression,
                        &config.seam_reconcile_layers,
                        &seam_metrics,
                    )?;
                    if encode_tx.send(encoded).is_err() {
                        break;
                    }
                }
                drop(encode_tx);

                let (features_read, max_batch_bytes, reader_ns) =
                    reader.join().expect("reader panicked")?;
                let (
                    tiles_written,
                    pmtiles,
                    tiles_per_zoom,
                    unique_per_zoom,
                    bytes_per_zoom,
                    size_diag,
                ) = writer.join().expect("writer panicked");
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
        eprintln!("assemble_reader_total_ns={total_ns}");
        eprintln!("assemble_partition_workers={workers}");
        eprintln!("assemble_partitions={partitions}");
    } else {
        eprintln!(
            "  Assemble reader thread (k-way merge): {:.1}s",
            reader_ns as f64 / 1_000_000_000.0
        );
    }
    eprintln!("assemble_reader_ns={reader_ns}");
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
    pmtiles.write_to(&config.output_path)?;

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
        eprintln!("seam_tiles_touched={seam_touched}");
        eprintln!("seam_rings_decoded={seam_rings}");
        eprintln!("seam_chains_detected={seam_chains}");
        eprintln!("seam_chains_reconciled={seam_reconciled}");
        eprintln!("seam_chains_skipped={seam_skipped}");
        eprintln!("seam_reconcile_us={seam_us}");
        eprintln!("seam_layers={}", seam_layer_descs.join("+"));
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
) -> Result<AssembleCore, PipelineError> {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::mpsc;

    let partition_count = partitions.len();
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

    let worker_count = config.threads.clamp(1, 4).min(partition_count);
    let next_job = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<Result<PartitionBatch, PipelineError>>();

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

    let mut features_read: u64 = 0;
    let mut tiles_written: u64 = 0;
    let mut tiles_per_zoom = [0u64; 15];
    let mut unique_per_zoom = [0u64; 15];
    let mut bytes_per_zoom = [0u64; 15];
    let mut max_batch_bytes: usize = 0;
    let mut size_diag = TileSizeDiagnostics::default();
    let mut reader_max_ns: u64 = 0;
    let mut reader_total_ns: u64 = 0;

    let scope_result: Result<(), PipelineError> = std::thread::scope(|s| {
        for _ in 0..worker_count {
            let tx = tx.clone();
            let partitions_ref = &partitions;
            let next_job_ref = &next_job;
            let stop_ref = &stop;
            let seam_layers = seam_reconcile_layers;
            s.spawn(move || {
                loop {
                    if stop_ref.load(Ordering::Relaxed) {
                        break;
                    }
                    let order = next_job_ref.fetch_add(1, Ordering::Relaxed);
                    if order >= partitions_ref.len() {
                        break;
                    }
                    let partition = &partitions_ref[order];
                    let result = read_encode_partition(
                        order,
                        partition,
                        compression,
                        compression_level,
                        tile_format,
                        tile_compression,
                        &seam_layers,
                        seam_metrics,
                        assemble_budget,
                        &tx,
                    );
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
        for result in rx {
            let batch = result?;
            if batch.order >= partition_count {
                return Err(PipelineError(format!(
                    "assemble partition batch has invalid order {} of {partition_count}",
                    batch.order
                )));
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
                features_read += batch.features_read;
                max_batch_bytes = max_batch_bytes.max(batch.max_batch_bytes);
                reader_total_ns += batch.reader_ns;
                current_partition_reader_ns += batch.reader_ns;
                for tile in batch.encoded_tiles {
                    let (z, x, y) = pmtiles_writer::tile_id_to_zxy(tile.tile_id);
                    let tile_bytes = tile.compressed.len() as u64;
                    record_tile_size_diagnostics(&mut size_diag, tile.tile_id, tile_bytes);
                    let is_unique = pmtiles.add_tile(z, x, y, &tile.compressed)?;
                    tiles_written += 1;
                    if (z as usize) < 15 {
                        tiles_per_zoom[z as usize] += 1;
                        if is_unique {
                            unique_per_zoom[z as usize] += 1;
                            bytes_per_zoom[z as usize] += tile_bytes;
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
                    pending.remove(&next_write);
                    next_write += 1;
                    next_batch = 0;
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
    });
    scope_result?;

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

fn encode_and_send_partition_batch(
    tx: &std::sync::mpsc::Sender<Result<PartitionBatch, PipelineError>>,
    ctx: &PartitionEncodeCtx<'_>,
    meta: &PartitionBatchMeta,
    pending_tiles: &[PendingTile],
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
    send_partition_batch(
        tx,
        PartitionBatch {
            order: meta.order,
            batch_index: meta.batch_index,
            is_last: meta.is_last,
            features_read: meta.features_read,
            max_batch_bytes: meta.max_batch_bytes,
            reader_ns: meta.reader_ns,
            encoded_tiles,
        },
    )
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

thread_local! {
    static ASSEMBLY_SCRATCH: std::cell::RefCell<AssemblyScratch> = std::cell::RefCell::new(
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
    );
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
            ASSEMBLY_SCRATCH.with(|cell| {
                let s = &mut *cell.borrow_mut();

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
                #[allow(clippy::cast_possible_truncation)]
                let level = match z {
                    0..=8 => compression_level.clamp(9, 10),
                    13..=14 => compression_level.min(3),
                    _ => compression_level,
                } as usize;

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
                s.gz_buf = Vec::with_capacity(compressed.len());

                Some(EncodedTile {
                    tile_id: tile.tile_id,
                    compressed,
                })
            })
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
            ASSEMBLY_SCRATCH.with(|cell| {
                let s = &mut *cell.borrow_mut();
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
            })
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
