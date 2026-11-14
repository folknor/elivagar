use std::sync::atomic::Ordering;

use crate::archive::{self, LonLatBbox, MapCenter, PmtilesConfig, PmtilesWriter, TileRun};
use crate::debug::{WAIT, emit_counter_u64, emit_counter_usize, wait_span};
#[cfg(feature = "mlt")]
use crate::mlt;
use crate::mvt::{self, LayerBuilder};
use crate::ocean::band::TileRect;
use crate::shortbread::{self, Layer};
use crate::sort;
use crate::tiling::{MAX_ZOOM, TileId, ZOOM_COUNT, zoom_ids};
use crate::wire_format::add_feature_to_layer;
use pbfhogg::ElementReader;

use super::stats::{TileSizeDiagnostics, record_tile_size_diagnostics};
use super::{
    PipelineError, TileCompression, TilePayloadFormat, TilegenConfig, compression_level_for_zoom,
    configured_tile_contract,
};

// ---------------------------------------------------------------------------
// Phase 4: Tile assembly + PMTiles write
// ---------------------------------------------------------------------------

/// A tile's features collected from the sort reader, ready for parallel encoding.
pub(super) struct PendingTile {
    pub(super) tile_id: TileId,
    pub(super) features: Vec<(u8, Box<[u8]>)>, // (layer_idx, feature_data)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnknownSortLayerId(u8);

fn sort_layer_error(tile: &PendingTile, error: UnknownSortLayerId) -> PipelineError {
    let (z, x, y) = tile.tile_id.coord();
    PipelineError(format!(
        "corrupt or foreign sort record has unknown layer id {} in tile {z}/{x}/{y}",
        error.0
    ))
}

const _: () = assert!(std::mem::size_of::<PendingTile>() == 32);

/// Brotli's I/O buffer size, independent of the tile coordinate grid.
const BROTLI_BUFFER_BYTES: usize = 4096;

/// A payload encoded under the run's tile contract, ready for PMTiles.
/// Keep the contract on the run rather than every tile: a writer handles one
/// contract, and the 32-byte batch item size is asserted below. Repeating the
/// contract here would enlarge every queued tile without validating its bytes.
pub(super) struct EncodedTile {
    pub(super) tile_id: TileId,
    pub(super) compressed: Vec<u8>,
}

/// Ordered work scheduled into the single PMTiles writer. `RunCopy` keeps a
/// shared artifact payload out of worker batches until the writer needs it.
pub(super) enum PartitionItem {
    Encoded(EncodedTile),
    RunCopy {
        tile_id: TileId,
        run_length: u32,
        offset: u64,
        length: u32,
    },
}

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
        let tile_id = sort::tile_id_from_key(key).raw();
        let bucket = usize::try_from((tile_id.saturating_sub(range_start)) >> shift)
            .map_err(|_| std::io::Error::other("piece scan bucket does not fit usize"))?;
        if bucket >= bucket_count {
            return Err(std::io::Error::other(format!(
                "piece scan found tile id {tile_id} outside its partition range in {}",
                source.path().display()
            )));
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
                        let source_index =
                            next_source.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
const _: () = assert!(std::mem::size_of::<EncodedTile>() == 32);

struct AssembleCore {
    features_read: u64,
    tiles_written: u64,
    pmtiles: PmtilesWriter,
    tiles_per_zoom: [u64; ZOOM_COUNT],
    unique_per_zoom: [u64; ZOOM_COUNT],
    bytes_per_zoom: [u64; ZOOM_COUNT],
    max_batch_bytes: usize,
    size_diag: TileSizeDiagnostics,
    reader_ns: u64,
    reader_total_ns: u64,
    partition_workers: usize,
    partition_count: usize,
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

struct PartitionEncodeCtx {
    compression_level: u32,
    tile_format: TilePayloadFormat,
    tile_compression: TileCompression,
}

struct ArtifactPartitionCtx<'a> {
    ocean: &'a crate::ocean::OceanTiles,
    band_empty: bool,
    copy_ranges_by_zoom: Vec<Vec<(u64, u64)>>,
}

impl<'a> ArtifactPartitionCtx<'a> {
    fn new(ocean: &'a crate::ocean::OceanTiles) -> Result<Self, PipelineError> {
        let band = ocean.band();
        let band_empty = band.is_empty();
        let mut copy_ranges_by_zoom = Vec::new();
        if !band_empty {
            copy_ranges_by_zoom.reserve(ZOOM_COUNT);
            for z in 0..=MAX_ZOOM {
                copy_ranges_by_zoom.push(artifact_copy_ranges(band, z)?);
            }
        }
        Ok(Self {
            ocean,
            band_empty,
            copy_ranges_by_zoom,
        })
    }

    /// The artifact run whose payload a surviving encoded tile takes its
    /// ocean layer from, or `None` when the computed pass owns the tile's
    /// ocean or the artifact holds nothing there.
    fn splice_run(&self, tile_id: TileId) -> Option<TileRun> {
        let (z, x, y) = tile_id.coord();
        if self.ocean.band().computed_owns(z, x, y) {
            return None;
        }
        self.ocean.run_covering(tile_id.raw())
    }
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

#[cfg(test)]
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
        archive::DedupStats,
        TileSizeDiagnostics,
    ),
    PipelineError,
> {
    let run = RunProvenance {
        input_identity: Some(crate::provenance::hash_file(&config.pbf_path)?),
        ..RunProvenance::default()
    };
    phase_assemble_with_ocean(
        sort_reader,
        config,
        &crate::resources::Resources::resolve(config)?,
        &crate::geometry::projection::MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        },
        None,
        run,
    )
}

/// Run facts that only the orchestrator knows, for archive provenance.
///
/// Passed in rather than re-derived here: `effective` is chosen in phase12
/// and is `None` when that phase did not run, and the artifact key belongs to
/// the resolved ocean mode. Re-deriving either in assemble would describe this
/// invocation rather than the run that produced the tiles.
#[derive(Debug)]
pub(super) struct RunProvenance {
    /// XXH3-128 and byte length of the input PBF, hashed once by the
    /// orchestrator rather than recomputed here. The checkpoint stores the
    /// digest for resume validation. `None` only for the ocean artifact builder.
    pub(super) input_identity: Option<crate::provenance::InputIdentity>,
    /// The paths phase12 actually took, or those the checkpoint recorded for
    /// a resumed run. `None` only when neither is available.
    pub(super) effective: Option<crate::provenance::Effective>,
    /// The phase a resumed run started at; `None` for a full run.
    pub(super) resumed_from: Option<super::SkipTo>,
    /// The resolved ocean producer, carrying its source identity.
    pub(super) ocean_mode: super::OceanMode,
    /// Which file served the low zooms, from the run's ocean plan; `None`
    /// exactly when `ocean_mode` is `None`.
    pub(super) low_zoom_source: Option<crate::provenance::ActiveLowZoomSource>,
}

impl Default for RunProvenance {
    fn default() -> Self {
        Self {
            input_identity: None,
            effective: None,
            resumed_from: None,
            ocean_mode: super::OceanMode::None,
            low_zoom_source: None,
        }
    }
}

/// Extract bounds include the checkpoint's ocean overlap buffer. Mercator y
/// increases southward, so the southwest and northeast corners swap y limits.
fn archive_config(
    config: &TilegenConfig,
    data_bounds: &crate::geometry::projection::MercBbox,
) -> std::io::Result<PmtilesConfig> {
    use crate::geometry::projection::{Point, unproject};

    let bounds = if config.ocean_only_metadata {
        LonLatBbox {
            west: -180.0,
            south: -85.05,
            east: 180.0,
            north: 85.05,
        }
    } else {
        let (west, south) = unproject(&Point::new(data_bounds.min_x, data_bounds.max_y));
        let (east, north) = unproject(&Point::new(data_bounds.max_x, data_bounds.min_y));
        LonLatBbox {
            west,
            south,
            east,
            north,
        }
    };
    let center = MapCenter {
        lon: (bounds.west + bounds.east) * 0.5,
        lat: (bounds.south + bounds.north) * 0.5,
        zoom: 2_u8.clamp(config.min_zoom, config.max_zoom),
    };
    PmtilesConfig::new(config.min_zoom, config.max_zoom, bounds, center)
}

#[allow(clippy::too_many_lines)]
// Both are moved into the phase body's closures under the hotpath feature,
// where the measure macro's wrapping consumes them; without it clippy sees
// only the borrow. Taking them by reference would push the clone to every
// caller instead.
#[allow(clippy::needless_pass_by_value)]
#[hotpath::measure]
pub(super) fn phase_assemble_with_ocean(
    sort_reader: &mut sort::SortReader,
    config: &TilegenConfig,
    resources: &crate::resources::Resources,
    data_bounds: &crate::geometry::projection::MercBbox,
    ocean_tiles: Option<std::sync::Arc<crate::ocean::OceanTiles>>,
    run: RunProvenance,
) -> Result<
    (
        u64,
        u64,
        u64,
        usize,
        archive::DedupStats,
        TileSizeDiagnostics,
    ),
    PipelineError,
> {
    let pbf_name = if config.ocean_only_metadata {
        if !matches!(&run.ocean_mode, super::OceanMode::Band { .. })
            || config.ocean_artifact_key.is_none()
        {
            return Err(PipelineError(
                "ocean artifact assembly requires an explicit artifact identity".to_string(),
            ));
        }
        None
    } else {
        let name = config
            .pbf_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                PipelineError(
                    "input PBF filename is unavailable for archive provenance".to_string(),
                )
            })?;
        if run.input_identity.is_none() {
            return Err(PipelineError(
                "input PBF identity is unavailable for archive provenance".to_string(),
            ));
        }
        Some(name.to_string())
    };
    let pmtiles_config = archive_config(config, data_bounds)?;
    let mut pmtiles = if config.in_memory {
        PmtilesWriter::new(pmtiles_config)
    } else {
        PmtilesWriter::new_streaming(pmtiles_config, &config.tmp_dir, &config.output_path)?
    };
    pmtiles.set_dedup_cap(resources.dedup_cap);
    let (tile_format, tile_compression) = configured_tile_contract(config);
    pmtiles.set_tile_contract(tile_format, tile_compression);
    if let Some(key) = &config.ocean_artifact_key {
        pmtiles.set_ocean_artifact_metadata(key.clone())?;
    }
    if config.ocean_only_metadata {
        pmtiles.set_ocean_only_metadata();
    }

    let partitions = sort_reader.take_partitions();
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
    } = phase_assemble_partitions(
        &partitions,
        pmtiles,
        config,
        resources,
        ocean_tiles.as_deref(),
    )?;
    eprintln!(
        "  Assemble partition readers: {} partitions, {} workers, max {:.1}s, total {:.1}s",
        partition_count,
        partition_workers,
        reader_ns as f64 / 1_000_000_000.0,
        reader_total_ns as f64 / 1_000_000_000.0
    );
    emit_counter_u64("assemble_reader_total_ns", reader_total_ns);
    emit_counter_usize("assemble_partition_workers", partition_workers);
    emit_counter_usize("assemble_partitions", partition_count);
    emit_counter_u64("assemble_reader_ns", reader_ns);
    if let Some(name) = pbf_name {
        pmtiles.set_source_pbf_filename(name.clone());
        let reader = ElementReader::from_path(&config.pbf_path).map_err(|error| {
            PipelineError(format!(
                "cannot read input PBF header for archive provenance: {error}"
            ))
        })?;
        let header = reader.header();
        let replication_timestamp = header.osmosis_replication_timestamp();
        let pbf_features = crate::provenance::PbfFeatures {
            sort_type_then_id: header.is_sorted(),
            locations_on_ways: header.has_locations_on_ways(),
            way_members_v1: header.has_way_members_v1(),
            shared_node_pins_v1: header.has_shared_node_pins_v1(),
        };
        if let Some(ts) = replication_timestamp {
            pmtiles.set_osmosis_replication_timestamp(ts);
        }
        write_provenance(
            &mut pmtiles,
            config,
            name,
            replication_timestamp,
            pbf_features,
            &run,
        )?;
    } else {
        // The artifact is identified by its unchanged top-level ocean_artifact
        // member and has no input PBF or run-shaped provenance to read/write.
        crate::debug::emit_counter("ocean_artifact_no_run_provenance", 1);
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
            tiles_written: 0,
            pmtiles,
            tiles_per_zoom: [0; ZOOM_COUNT],
            unique_per_zoom: [0; ZOOM_COUNT],
            bytes_per_zoom: [0; ZOOM_COUNT],
            max_batch_bytes: 0,
            size_diag: TileSizeDiagnostics::default(),
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
    let compression_level = config.compression_level;
    let tile_format = config.tile_format;
    let tile_compression = config.tile_compression;
    let assemble_budget = resources.assemble_batch;
    let artifact_ctx = ocean.map(ArtifactPartitionCtx::new).transpose()?;

    let mut features_read: u64 = 0;
    let mut tiles_written: u64 = 0;
    let mut tiles_per_zoom = [0u64; ZOOM_COUNT];
    let mut unique_per_zoom = [0u64; ZOOM_COUNT];
    let mut bytes_per_zoom = [0u64; ZOOM_COUNT];
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
                                        compression_level,
                                        tile_format,
                                        tile_compression,
                                        assemble_budget,
                                        artifact_ctx,
                                        &tx,
                                    )
                                })
                        }
                        UnionPartition::SortPiece(piece) => {
                            sort::SortPartitionReader::open_slices(&piece.sources, compression)
                                .map_err(PipelineError::from)
                                .and_then(|reader| {
                                    read_encode_partition(
                                        order,
                                        reader,
                                        (piece.start_tile, piece.end_tile),
                                        compression_level,
                                        tile_format,
                                        tile_compression,
                                        assemble_budget,
                                        artifact_ctx,
                                        &tx,
                                    )
                                })
                        }
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
                                    &ocean.raw_blob(archive::BlobRef { offset, length })?,
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

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn read_encode_partition(
    order: usize,
    mut reader: sort::SortPartitionReader,
    tile_range: (u64, u64),
    compression_level: u32,
    tile_format: TilePayloadFormat,
    tile_compression: TileCompression,
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
    // reader; a scoped encoder thread runs the rayon encode + artifact splice
    // + send. sync_channel(1) lets the reader pull batch N+1 while batch N
    // encodes - previously the reader sat idle through every encode. RAM
    // bound: one extra raw batch (assemble_budget, 32 MB default) in flight
    // per worker. Batch order within the partition is preserved by channel
    // FIFO; the artifact cursor lives on the encoder thread where batches
    // stay sequential. Error cascade: an encoder error drops batch_rx, the
    // reader's send fails and it stops; a reader error drops batch_tx and
    // the encoder drains out - either error surfaces after the join.
    std::thread::scope(|scope| -> Result<(), PipelineError> {
        let (batch_tx, batch_rx) = std::sync::mpsc::sync_channel::<RawPartitionBatch>(1);
        let writer_tx = tx.clone();
        let encoder = scope.spawn(move || -> Result<(), PipelineError> {
            let encode_ctx = PartitionEncodeCtx {
                compression_level,
                tile_format,
                tile_compression,
            };
            let mut artifact_cursor = ArtifactCursor {
                next: range_start,
                end: partition_end,
            };
            while let Ok((meta, batch)) = batch_rx.recv() {
                encode_and_send_partition_batch(
                    &writer_tx,
                    &encode_ctx,
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

                let tile_id = sort::tile_id_from_key(r.key);
                let layer_idx = sort::layer_from_key(r.key);
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
                current.features.push((layer_idx, r.data));
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

/// Write the `elivagar` provenance member describing the contract this
/// archive was built under.
///
/// Every fact here is taken from the resolved run state rather than
/// re-derived from config: the ocean mode and low-zoom source come from the
/// ocean plan `run` resolved once, which is what the passes and the artifact
/// admission actually used. Metadata that overstates is worse than metadata
/// that is absent.
///
/// Ordinary run archives require the input name and identity. The ocean
/// artifact builder bypasses this function because it has no PBF input.
fn write_provenance(
    pmtiles: &mut PmtilesWriter,
    config: &TilegenConfig,
    name: String,
    replication_timestamp: Option<i64>,
    features: crate::provenance::PbfFeatures,
    run: &RunProvenance,
) -> Result<(), PipelineError> {
    let identity = run.input_identity.ok_or_else(|| {
        PipelineError("input PBF identity is unavailable for archive provenance".to_string())
    })?;
    let input = crate::provenance::Input {
        name,
        identity,
        replication_timestamp,
        features,
    };
    let ocean = crate::provenance::OceanContract {
        kind: run.ocean_mode.contract_kind(run.low_zoom_source)?,
        // Invariably true since --no-ocean-simplify was removed. The field
        // stays because it is part of the comparability contract and archives
        // built before that removal can carry false: dropping it would make
        // those archives incomparable to these, which is the opposite of what
        // the block is for.
        runtime_simplification: true,
    };
    let value = crate::provenance::build(&input, config, &ocean, run.effective, run.resumed_from);
    pmtiles.set_elivagar_metadata(value)?;
    Ok(())
}

fn artifact_run_copy_items(
    start: u64,
    end: u64,
    artifact: &ArtifactPartitionCtx<'_>,
) -> Result<Vec<PartitionItem>, PipelineError> {
    let mut items = Vec::new();
    if start >= end {
        return Ok(items);
    }
    for run in artifact.ocean.runs_in(start, end) {
        let blob = run.blob();
        let mut cursor = start.max(run.start().raw());
        let run_end = end.min(run.end());
        if artifact.band_empty {
            // Preserve zoom boundaries for the per-zoom writer counters.
            // With an empty band, every tile in the source run is copied.
            while cursor < run_end {
                let z = artifact_tile(cursor)?.zoom();
                let zoom_end = zoom_ids(z).1;
                let sub_end = run_end.min(zoom_end);
                append_artifact_run_copy(&mut items, cursor, sub_end, blob.offset, blob.length)?;
                cursor = sub_end;
            }
            continue;
        }

        // `OceanBand::computed_owns` is the complement of one axis-aligned
        // interior tile rectangle for each zoom (`artifact_tile_rect`). Those
        // rectangles are converted once into Hilbert ID ranges in
        // `ArtifactPartitionCtx`; intersecting them with this source run
        // avoids decoding every tile ID in long artifact runs.
        while cursor < run_end {
            let z = artifact_tile(cursor)?.zoom();
            let zoom_end = zoom_ids(z).1;
            let sub_end = run_end.min(zoom_end);
            let ranges = &artifact.copy_ranges_by_zoom[z as usize];
            let first = ranges.partition_point(|range| range.1 <= cursor);
            for &(range_start, range_end) in &ranges[first..] {
                if range_start >= sub_end {
                    break;
                }
                append_artifact_run_copy(
                    &mut items,
                    range_start.max(cursor),
                    range_end.min(sub_end),
                    blob.offset,
                    blob.length,
                )?;
            }
            cursor = sub_end;
        }
    }
    Ok(items)
}

/// The artifact's runs are checked against the profile domain when it is
/// opened (`OceanTiles::open`), so a raw id inside one is a profile tile.
fn artifact_tile(raw: u64) -> Result<TileId, PipelineError> {
    TileId::new(raw)
        .filter(|tile| tile.is_profile())
        .ok_or_else(|| {
            PipelineError(format!(
                "ocean artifact tile id {raw} lies beyond z{MAX_ZOOM}"
            ))
        })
}

fn append_artifact_run_copy(
    items: &mut Vec<PartitionItem>,
    start: u64,
    end: u64,
    offset: u64,
    length: u32,
) -> Result<(), PipelineError> {
    if start >= end {
        return Ok(());
    }
    items.push(PartitionItem::RunCopy {
        tile_id: artifact_tile(start)?,
        run_length: u32::try_from(end - start)
            .map_err(|_| PipelineError("artifact split run length exceeds u32".to_string()))?,
        offset,
        length,
    });
    Ok(())
}

/// Hilbert id ranges of the tiles the artifact owns at one zoom: the band's
/// artifact rectangle, merged into maximal spans.
fn artifact_copy_ranges(
    band: &crate::ocean::OceanBand,
    z: u8,
) -> Result<Vec<(u64, u64)>, PipelineError> {
    let side = 1_u32 << z;
    let Some(rect) = band.artifact_tile_rect(z) else {
        return Ok(Vec::new());
    };

    let mut ranges = Vec::new();
    collect_hilbert_rect_ranges(z, rect, 0, 0, side, &mut ranges)?;
    ranges.sort_unstable_by_key(|range| range.0);

    // Adjacent quadtree cells form one contiguous Hilbert interval. Merge
    // them so every emitted RunCopy remains a maximal span within its zoom.
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    Ok(merged)
}

fn collect_hilbert_rect_ranges(
    z: u8,
    rect: TileRect,
    x: u32,
    y: u32,
    side: u32,
    ranges: &mut Vec<(u64, u64)>,
) -> Result<(), PipelineError> {
    let end_x = x + side;
    let end_y = y + side;
    if end_x <= rect.min_x || x >= rect.max_x || end_y <= rect.min_y || y >= rect.max_y {
        return Ok(());
    }
    if rect.min_x <= x && rect.min_y <= y && rect.max_x >= end_x && rect.max_y >= end_y {
        let last_x = end_x - 1;
        let last_y = end_y - 1;
        let start = [
            TileId::from_coord(z, x, y)?,
            TileId::from_coord(z, last_x, y)?,
            TileId::from_coord(z, x, last_y)?,
            TileId::from_coord(z, last_x, last_y)?,
        ]
        .into_iter()
        .min()
        .expect("four tile corners")
        .raw();
        let length = u64::from(side) * u64::from(side);
        ranges.push((start, start + length));
        return Ok(());
    }
    if side == 1 {
        return Ok(());
    }

    let half = side / 2;
    for (child_x, child_y) in [(x, y), (x + half, y), (x + half, y + half), (x, y + half)] {
        collect_hilbert_rect_ranges(z, rect, child_x, child_y, half, ranges)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_run_copy(
    pmtiles: &mut PmtilesWriter,
    tile_id: TileId,
    run_length: u32,
    data: &[u8],
    tiles_written: &mut u64,
    tiles_per_zoom: &mut [u64; ZOOM_COUNT],
    unique_per_zoom: &mut [u64; ZOOM_COUNT],
    bytes_per_zoom: &mut [u64; ZOOM_COUNT],
    size_diag: &mut TileSizeDiagnostics,
) -> Result<(), PipelineError> {
    let z = tile_id.zoom();
    let bytes = u64::try_from(data.len())
        .map_err(|_| PipelineError("tile payload length exceeds u64".to_string()))?;
    let unique = pmtiles.add_run(tile_id, run_length, data)?;
    let tile_id = tile_id.raw();
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
    tiles_per_zoom: &mut [u64; ZOOM_COUNT],
    unique_per_zoom: &mut [u64; ZOOM_COUNT],
    bytes_per_zoom: &mut [u64; ZOOM_COUNT],
    size_diag: &mut TileSizeDiagnostics,
) -> Result<(), PipelineError> {
    let (z, x, y) = tile.tile_id.coord();
    let bytes = tile.compressed.len() as u64;
    record_tile_size_diagnostics(size_diag, tile.tile_id.raw(), bytes);
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
    Ok(crate::mvt::layers::decode_mvt_layers(raw)?
        .iter()
        .any(|layer| layer.name == Layer::Ocean.name()))
}

fn ocean_layer_field(compressed: &[u8]) -> Result<Vec<u8>, PipelineError> {
    let raw = crate::archive::gzip_decompress(compressed)?;
    let layers = crate::mvt::layers::decode_mvt_layers(&raw)?;
    if layers.len() != 1 || layers[0].name != Layer::Ocean.name() {
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
    tile_id: TileId,
    compression_level: u32,
) -> Result<Vec<u8>, PipelineError> {
    raw.extend_from_slice(field);
    let z = tile_id.zoom();
    let level = compression_level_for_zoom(z, compression_level);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
    use std::io::Write;
    encoder.write_all(&raw)?;
    Ok(encoder.finish()?)
}

/// Where artifact gap filling stands within one partition or piece: every
/// address below `next` has been planned, and `end` is the exclusive end of
/// the range this reader owns.
struct ArtifactCursor {
    next: u64,
    end: u64,
}

/// One output item of an artifact-served batch, in address order.
enum PlannedItem {
    Copy(PartitionItem),
    /// A surviving encoded tile and the artifact run whose ocean layer it
    /// takes, if any.
    Encoded {
        tile: EncodedTile,
        splice: Option<TileRun>,
    },
}

/// Interleave a batch's surviving encoded tiles with the artifact run copies
/// that fill the addresses between them.
///
/// Only encoded tiles reserve an address. A pending tile whose layers all
/// came out empty encodes to nothing, and its address falls inside a copy
/// range like any other gap, so a tile the artifact owns keeps its ocean
/// whether or not OSM content survived there. On the last batch the tail up to
/// the range end is copied and the cursor closes.
fn plan_artifact_items(
    artifact: &ArtifactPartitionCtx<'_>,
    cursor: &mut ArtifactCursor,
    encoded: Vec<EncodedTile>,
    is_last: bool,
) -> Result<Vec<PlannedItem>, PipelineError> {
    let mut items = Vec::with_capacity(encoded.len());
    for tile in encoded {
        let id = tile.tile_id.raw();
        // Encoded ids come from this reader's own sorted input, so they are
        // strictly increasing and inside its range; anything else would make
        // the copies below overlap or reorder the writer's stream.
        if id < cursor.next || id >= cursor.end {
            return Err(PipelineError(format!(
                "encoded tile {id} lies outside the unplanned partition range {}..{}",
                cursor.next, cursor.end
            )));
        }
        items.extend(
            artifact_run_copy_items(cursor.next, id, artifact)?
                .into_iter()
                .map(PlannedItem::Copy),
        );
        let splice = artifact.splice_run(tile.tile_id);
        items.push(PlannedItem::Encoded { tile, splice });
        cursor.next = id + 1;
    }
    if is_last {
        items.extend(
            artifact_run_copy_items(cursor.next, cursor.end, artifact)?
                .into_iter()
                .map(PlannedItem::Copy),
        );
        cursor.next = cursor.end;
    }
    Ok(items)
}

fn encode_and_send_partition_batch(
    tx: &std::sync::mpsc::Sender<Result<PartitionBatch, PipelineError>>,
    ctx: &PartitionEncodeCtx,
    meta: &PartitionBatchMeta,
    pending_tiles: &[PendingTile],
    artifact: Option<&ArtifactPartitionCtx<'_>>,
    artifact_cursor: &mut ArtifactCursor,
) -> Result<(), PipelineError> {
    let encoded_tiles = if pending_tiles.is_empty() {
        Vec::new()
    } else {
        encode_tile_batch(
            pending_tiles,
            ctx.compression_level,
            ctx.tile_format,
            ctx.tile_compression,
        )?
    };
    let items = if let Some(artifact) = artifact {
        let planned = plan_artifact_items(artifact, artifact_cursor, encoded_tiles, meta.is_last)?;
        let mut items = Vec::with_capacity(planned.len());
        for item in planned {
            match item {
                PlannedItem::Copy(item) => items.push(item),
                PlannedItem::Encoded { mut tile, splice } => {
                    if let Some(run) = splice {
                        splice_artifact_ocean(&mut tile, artifact, run, ctx.compression_level)?;
                    }
                    items.push(PartitionItem::Encoded(tile));
                }
            }
        }
        items
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

/// Append the artifact's ocean layer to an encoded tile the artifact owns.
/// The tile must not already carry ocean: the producer prunes artifact-owned
/// cells, so computed ocean here means producer and consumer disagree on
/// ownership, and appending would draw the ocean twice.
fn splice_artifact_ocean(
    encoded: &mut EncodedTile,
    artifact: &ArtifactPartitionCtx<'_>,
    run: TileRun,
    compression_level: u32,
) -> Result<(), PipelineError> {
    let raw = crate::archive::gzip_decompress(&encoded.compressed)?;
    if raw_contains_ocean(&raw)? {
        return Err(PipelineError(
            "non-band OSM tile unexpectedly contains computed ocean".to_string(),
        ));
    }
    let ocean_field = ocean_layer_field(&artifact.ocean.raw_blob(run.blob())?)?;
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

/// Assembly state created for one tile. Pools reuse buffers during that
/// tile's preparation and encoding; dropping them bounds retention across tiles.
pub(super) struct AssemblyScratch {
    pub(super) encode_scratch: mvt::EncodeScratch,
    pub(super) merge_scratch: mvt::MergeScratch,
    pub(super) line_merge_scratch: mvt::LineMergeScratch,
    pub(super) geom_pool: Vec<Vec<u32>>,
    pub(super) tags_pool: Vec<Vec<(u32, u32)>>,
    pub(super) compression_levels: [Option<flate2::Compression>; 11],
    pub(super) gz_buf: Vec<u8>,
    pub(super) mvt_buf: Vec<u8>,
    pub(super) layers: [Option<LayerBuilder>; LAYER_COUNT],
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
        }
    }
}

/// Encode + compress a batch of tiles in parallel using rayon.
#[hotpath::measure]
#[allow(clippy::cast_possible_wrap)]
pub(super) fn encode_tile_batch(
    batch: &[PendingTile],
    compression_level: u32,
    tile_format: TilePayloadFormat,
    tile_compression: TileCompression,
) -> Result<Vec<EncodedTile>, PipelineError> {
    match super::TileContract::from_fields(tile_format, tile_compression, compression_level) {
        super::TileContract::Mvt { compression, level } => {
            encode_tile_batch_mvt(batch, level, compression)
        }
        #[cfg(feature = "mlt")]
        super::TileContract::Mlt => encode_tile_batch_mlt(batch),
        // Unreachable in practice: the CLI refuses --tile-format mlt without
        // the feature. A library caller can still construct the config, so
        // this fails loudly rather than silently writing MVT under an MLT
        // tile contract.
        #[cfg(not(feature = "mlt"))]
        super::TileContract::Mlt => Err(PipelineError(
            "tile format mlt requires a build with the `mlt` cargo feature".to_string(),
        )),
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
) -> Result<Vec<EncodedTile>, PipelineError> {
    use rayon::prelude::*;

    let results: Vec<Result<Option<EncodedTile>, PipelineError>> = batch
        .par_iter()
        .map(|tile| {
            {
                let mut scratch = AssemblyScratch::new();
                let s = &mut scratch;

                let z = tile.tile_id.zoom();
                prepare_tile_layers(s, tile).map_err(|error| sort_layer_error(tile, error))?;
                // Borrow only the layer field so encoding can use its own buffers.
                let non_empty = non_empty_layers_in_tile_order(&s.layers);
                if non_empty.is_empty() {
                    return Ok(None);
                }

                mvt::encode_tile_into(&mut s.mvt_buf, &non_empty, &mut s.encode_scratch);

                if s.mvt_buf.is_empty() {
                    return Ok(None);
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
                        let mut encoder = brotli::CompressorWriter::new(
                            &mut compress_buf,
                            BROTLI_BUFFER_BYTES,
                            quality,
                            22,
                        );
                        std::io::Write::write_all(&mut encoder, &s.mvt_buf)
                            .expect("brotli compress failed");
                        drop(encoder);
                        compress_buf
                    }
                };
                Ok(Some(EncodedTile {
                    tile_id: tile.tile_id,
                    compressed,
                }))
            }
        })
        .collect();

    let mut out = Vec::with_capacity(results.len());
    for result in results {
        if let Some(tile) = result? {
            out.push(tile);
        }
    }
    Ok(out)
}

/// Both encoders consume this stage so format selection cannot change the
/// geometry merge policy.
fn prepare_tile_layers(
    s: &mut AssemblyScratch,
    tile: &PendingTile,
) -> Result<(), UnknownSortLayerId> {
    // Reset persisted layers from previous tile (reclaim features + clear interning).
    for slot in &mut s.layers {
        if let Some(lb) = slot.as_mut() {
            lb.prepare_for_reuse(&mut s.geom_pool, &mut s.tags_pool);
        }
    }

    for &(layer_idx, ref data) in &tile.features {
        let layer = Layer::ALL
            .get(usize::from(layer_idx))
            .copied()
            .ok_or(UnknownSortLayerId(layer_idx))?;
        add_feature_to_layer(
            get_or_create_layer(&mut s.layers, usize::from(layer as u8)),
            data,
            &mut s.geom_pool,
            &mut s.tags_pool,
        );
    }

    for (li, layer) in s.layers.iter_mut().enumerate() {
        // Preserve the existing MVT ocean feature layout: it is also stored
        // in the durable world artifact. Removing this exclusion changes
        // those bytes and requires an OCEAN_POLICY_VERSION bump in ocean/mod.rs
        // and an artifact rebuild. This is a compatibility constraint, not
        // a topology requirement: the merger concatenates complete polygons,
        // and MapLibre's 500-ring cap applies per polygon, not per feature.
        if li == shortbread::Layer::Ocean as usize {
            continue;
        }
        if let Some(lb) = layer.as_mut() {
            lb.merge_same_attr_geometries(&mut s.merge_scratch, &mut s.geom_pool, &mut s.tags_pool);
        }
    }

    // Full-resolution top-zoom lines retain the existing MVT policy. Below
    // it, join through degree-2 endpoints with the same clip-boundary
    // protection in both formats; the merger itself selects line geometries.
    if tile.tile_id.zoom() < MAX_ZOOM {
        for lb in s.layers.iter_mut().flatten() {
            lb.merge_connected_lines(&mut s.line_merge_scratch);
        }
    }
    Ok(())
}

/// Build non-empty per-layer builders for the MLT encoder and model tests.
#[cfg(test)]
pub(super) fn prepare_non_empty_layers<'a>(
    s: &'a mut AssemblyScratch,
    tile: &PendingTile,
) -> Vec<&'a LayerBuilder> {
    prepare_non_empty_layers_checked(s, tile).expect("tile contains an unknown sort layer id")
}

#[cfg(any(feature = "mlt", test))]
fn prepare_non_empty_layers_checked<'a>(
    s: &'a mut AssemblyScratch,
    tile: &PendingTile,
) -> Result<Vec<&'a LayerBuilder>, UnknownSortLayerId> {
    prepare_tile_layers(s, tile)?;
    Ok(non_empty_layers_in_tile_order(&s.layers))
}

fn non_empty_layers_in_tile_order(layers: &[Option<LayerBuilder>]) -> Vec<&LayerBuilder> {
    // Layer::ALL is the explicit archived tile order, shared by the MVT and
    // MLT encoders. The array slots remain keyed by the layer's wire ID.
    Layer::ALL
        .iter()
        .filter_map(|layer| layers.get(*layer as usize).and_then(Option::as_ref))
        .filter(|layer| !layer.is_empty())
        .collect()
}

/// Encode a batch of tiles as MLT via `mlt_core`. Uncompressed payloads: the
/// MLT tile contract carries `TileDataCompression::None`.
#[cfg(feature = "mlt")]
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
                let non_empty = prepare_non_empty_layers_checked(s, tile)
                    .map_err(|error| sort_layer_error(tile, error))?;
                if non_empty.is_empty() {
                    return Ok(None);
                }
                let encoded = mlt::encode_tile(&non_empty).map_err(|err| {
                    let (z, x, y) = tile.tile_id.coord();
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

// Both payload encoders consume builders in Shortbread tile order, and
// artifact splicing appends the raw ocean field. Keep the shared order checked.
const _: () = {
    assert!(Layer::Ocean.tile_order() as usize == LAYER_COUNT - 1);
    assert!(Layer::ALL[LAYER_COUNT - 1] as u8 == Layer::Ocean as u8);
};

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

    /// A sparse world artifact for the consumer tests: zooms 0-2 in one run
    /// that crosses two zoom boundaries, z3 absent, a run from inside z4 to
    /// inside z6, and alternating present and absent blocks through z9.
    fn sparse_artifact_runs() -> Vec<(u64, u32)> {
        let mut runs = vec![(0, u32::try_from(zoom_ids(3).0).expect("run fits u32"))];
        let z4 = zoom_ids(4).0;
        let z6 = zoom_ids(6).0;
        runs.push((
            z4 + 37,
            u32::try_from(z6 + 1_000 - (z4 + 37)).expect("run fits u32"),
        ));
        let mut start = zoom_ids(7).0 + 11;
        let end = zoom_ids(9).1;
        while start < end {
            let length = 500.min(end - start);
            runs.push((start, u32::try_from(length).expect("run fits u32")));
            start += 2 * length;
        }
        runs
    }

    fn write_sparse_artifact(
        path: &std::path::Path,
        key: &crate::ocean::OceanArtifactKey,
        runs: &[(u64, u32)],
    ) {
        let mut writer = archive::PmtilesWriter::new(
            archive::PmtilesConfig::new(
                0,
                MAX_ZOOM,
                LonLatBbox {
                    west: -180.0,
                    south: -85.05,
                    east: 180.0,
                    north: 85.05,
                },
                MapCenter {
                    lon: 0.0,
                    lat: 0.0,
                    zoom: 2,
                },
            )
            .expect("valid archive config"),
        );
        writer.set_ocean_only_metadata();
        writer
            .set_ocean_artifact_metadata(key.clone())
            .expect("set artifact metadata");
        for (index, &(start, length)) in runs.iter().enumerate() {
            // Planning reads directory runs only; payload bytes are opaque.
            let payload = [u8::try_from(index % 251).expect("payload byte")];
            writer
                .add_run(
                    TileId::new(start).expect("artifact run id"),
                    length,
                    &payload,
                )
                .expect("add artifact run");
        }
        writer.write_to(path).expect("write artifact");
    }

    fn test_artifact_key() -> crate::ocean::OceanArtifactKey {
        crate::ocean::OceanArtifactKey {
            source: crate::ocean::OceanSourceKey {
                full_shp_xxh128: crate::ocean::Xxh128::from_u128(1),
                full_shx_xxh128: crate::ocean::Xxh128::from_u128(2),
                simplified_shp_xxh128: None,
                simplified_shx_xxh128: None,
                min_zoom: 0,
                max_zoom: MAX_ZOOM,
                policy_version: crate::ocean::OCEAN_POLICY_VERSION,
            },
            compression_level: 6,
        }
    }

    /// One planned output address: whether it came from a run copy, and for
    /// an encoded tile whether it takes the artifact's ocean.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Served {
        Copied,
        Encoded { spliced: bool },
    }

    fn expand(items: Vec<PlannedItem>, out: &mut Vec<(u64, Served)>) {
        for item in items {
            match item {
                PlannedItem::Copy(PartitionItem::RunCopy {
                    tile_id,
                    run_length,
                    ..
                }) => {
                    let start = tile_id.raw();
                    for id in start..start + u64::from(run_length) {
                        out.push((id, Served::Copied));
                    }
                }
                PlannedItem::Copy(PartitionItem::Encoded(_)) => {
                    panic!("planner copies are run copies")
                }
                PlannedItem::Encoded { tile, splice } => out.push((
                    tile.tile_id.raw(),
                    Served::Encoded {
                        spliced: splice.is_some(),
                    },
                )),
            }
        }
    }

    /// Plan `[start, end)` as one reader that sees `encoded` in batches of
    /// `batch` tiles, closing with an empty final batch.
    fn plan_reader(
        ctx: &ArtifactPartitionCtx<'_>,
        start: u64,
        end: u64,
        encoded: &[u64],
        batch: usize,
        out: &mut Vec<(u64, Served)>,
    ) {
        let mut cursor = ArtifactCursor { next: start, end };
        let tiles: Vec<u64> = encoded
            .iter()
            .copied()
            .filter(|&id| start <= id && id < end)
            .collect();
        for chunk in tiles.chunks(batch.max(1)) {
            let chunk = chunk
                .iter()
                .map(|&id| EncodedTile {
                    tile_id: tile(id),
                    compressed: Vec::new(),
                })
                .collect();
            expand(
                plan_artifact_items(ctx, &mut cursor, chunk, false).expect("plan batch"),
                out,
            );
        }
        expand(
            plan_artifact_items(ctx, &mut cursor, Vec::new(), true).expect("plan final batch"),
            out,
        );
        assert_eq!(
            cursor.next, end,
            "the final batch closes the reader's range"
        );
    }

    /// The consumer half of ocean ownership: the per-zoom Hilbert copy
    /// ranges, run copies and splice decisions, driven from one band over a
    /// sparse artifact, against an independent model of what each address
    /// must hold. The producer half is `computed_owns` itself, which the
    /// pyramid traversal filter calls.
    #[test]
    fn artifact_consumer_serves_every_owned_address_exactly_once() {
        use crate::geometry::projection::MercBbox;

        let dir = tempfile::tempdir().expect("create artifact directory");
        let path = dir.path().join("ocean.pmtiles");
        let key = test_artifact_key();
        let runs = sparse_artifact_runs();
        write_sparse_artifact(&path, &key, &runs);
        let end = zoom_ids(9).1;
        let present = |id: u64| {
            runs.iter()
                .any(|&(start, length)| start <= id && id < start + u64::from(length))
        };
        // Surviving encoded tiles, with gaps standing for addresses with no
        // OSM input and for pending tiles that encoded to nothing.
        let encoded: Vec<u64> = (0..end).filter(|id| id % 7 == 3 || id % 113 == 0).collect();

        for bounds in [
            MercBbox {
                min_x: 0.31,
                min_y: 0.28,
                max_x: 0.62,
                max_y: 0.71,
            },
            MercBbox {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 1.0,
                max_y: 1.0,
            },
            MercBbox {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 0.62,
                max_y: 0.71,
            },
            MercBbox {
                min_x: 0.31,
                min_y: 0.28,
                max_x: 1.0,
                max_y: 1.0,
            },
        ] {
            let band = crate::ocean::OceanBand::for_bounds(&bounds);
            let tiles = crate::ocean::OceanArtifact::admit(&path, 6, || Ok(key.clone()))
                .and_then(|artifact| artifact.into_tiles(band))
                .expect("admit sparse artifact");
            let ctx = ArtifactPartitionCtx::new(&tiles).expect("artifact partition ctx");

            let mut whole = Vec::new();
            plan_reader(&ctx, 0, end, &encoded, usize::MAX, &mut whole);
            let mut batched = Vec::new();
            plan_reader(&ctx, 0, end, &encoded, 97, &mut batched);
            // Adjacent pieces, one cut inside the z4-z6 run and one inside a
            // z8 block, each with its own cursor.
            let cuts = [0, zoom_ids(5).0 + 3, zoom_ids(8).0 + 250, end];
            let mut pieces = Vec::new();
            for pair in cuts.windows(2) {
                plan_reader(&ctx, pair[0], pair[1], &encoded, 41, &mut pieces);
            }
            assert_eq!(
                whole, batched,
                "batching must not change output in {bounds:?}"
            );
            assert_eq!(
                whole, pieces,
                "piece cuts must not change output in {bounds:?}"
            );
            assert!(
                whole.windows(2).all(|pair| pair[0].0 < pair[1].0),
                "output addresses must be strictly increasing in {bounds:?}"
            );

            let mut served = whole.into_iter().peekable();
            for id in 0..end {
                let (z, x, y) = tile(id).coord();
                let computed = band.computed_owns(z, x, y);
                let artifact_ocean = present(id) && !computed;
                let expected = if encoded.binary_search(&id).is_ok() {
                    Some(Served::Encoded {
                        spliced: artifact_ocean,
                    })
                } else {
                    artifact_ocean.then_some(Served::Copied)
                };
                let actual = served.next_if(|&(at, _)| at == id).map(|(_, how)| how);
                assert_eq!(
                    actual, expected,
                    "address z{z}/{x}/{y} in {bounds:?} (computed owns: {computed})"
                );
            }
            assert!(
                served.next().is_none(),
                "no output past the range in {bounds:?}"
            );
        }
    }

    /// The production encode path drops a pending tile whose layers all come
    /// out empty. Its address must still receive the artifact's ocean.
    #[test]
    fn a_pending_tile_that_encodes_to_nothing_keeps_its_artifact_ocean() {
        use crate::geometry::projection::MercBbox;

        let dir = tempfile::tempdir().expect("create artifact directory");
        let path = dir.path().join("ocean.pmtiles");
        let key = test_artifact_key();
        let address = TileId::at(2, 1, 1);
        write_sparse_artifact(&path, &key, &[(address.raw(), 1)]);
        let world = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let tiles = crate::ocean::OceanArtifact::admit(&path, 6, || Ok(key.clone()))
            .and_then(|artifact| artifact.into_tiles(crate::ocean::OceanBand::for_bounds(&world)))
            .expect("admit artifact");
        let ctx = ArtifactPartitionCtx::new(&tiles).expect("artifact partition ctx");
        // A pending tile with no features prepares no layer and encodes to
        // nothing, the same outcome as features that all drop.
        let pending = [PendingTile {
            tile_id: address,
            features: Vec::new(),
        }];
        assert!(
            encode_tile_batch(&pending, 6, TilePayloadFormat::Mvt, TileCompression::Gzip)
                .expect("encode batch")
                .is_empty()
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let mut cursor = ArtifactCursor {
            next: 0,
            end: zoom_ids(2).1,
        };
        encode_and_send_partition_batch(
            &tx,
            &PartitionEncodeCtx {
                compression_level: 6,
                tile_format: TilePayloadFormat::Mvt,
                tile_compression: TileCompression::Gzip,
            },
            &PartitionBatchMeta {
                order: 0,
                batch_index: 0,
                is_last: true,
                features_read: 0,
                max_batch_bytes: 0,
                reader_ns: 0,
            },
            &pending,
            Some(&ctx),
            &mut cursor,
        )
        .expect("encode and send");
        let batch = rx.recv().expect("batch sent").expect("batch ok");
        let copied: Vec<(u64, u32)> = batch
            .items
            .iter()
            .filter_map(|item| match item {
                PartitionItem::RunCopy {
                    tile_id,
                    run_length,
                    ..
                } => Some((tile_id.raw(), *run_length)),
                PartitionItem::Encoded(_) => None,
            })
            .collect();
        assert_eq!(copied, vec![(address.raw(), 1)]);
    }

    #[test]
    fn shared_preparation_preserves_ocean_and_line_zoom_policy() {
        for z in [13, 14] {
            let mut tile = PendingTile {
                tile_id: TileId::at(z, 0, 0),
                features: Vec::new(),
            };
            for coords in [[(100, 100), (200, 100)], [(200, 100), (300, 100)]] {
                let mut geometry = Vec::new();
                mvt::encode_linestring(&mut geometry, &coords);
                tile.features.push((
                    Layer::Boundaries as u8,
                    crate::wire_format::encode_feature_data_with_attrs(
                        1,
                        mvt::GeomType::LineString,
                        &geometry,
                        &[0],
                    ),
                ));
            }
            let mut polygon = Vec::new();
            mvt::encode_polygon(
                &mut polygon,
                &[&[(100, 100), (200, 100), (200, 200), (100, 100)]],
            );
            for _ in 0..2 {
                tile.features.push((
                    Layer::Ocean as u8,
                    crate::wire_format::encode_feature_data_with_attrs(
                        2,
                        mvt::GeomType::Polygon,
                        &polygon,
                        &[0],
                    ),
                ));
            }
            let mut scratch = AssemblyScratch::new();
            let layers = prepare_non_empty_layers(&mut scratch, &tile);
            assert_eq!(layers.len(), 2);
            let boundaries = scratch.layers[Layer::Boundaries as usize].as_ref().unwrap();
            assert_eq!(boundaries.features.len(), 1);
            let mut expected = Vec::new();
            if z < 14 {
                mvt::encode_linestring(&mut expected, &[(100, 100), (200, 100), (300, 100)]);
            } else {
                // Two unjoined sub-lines remain in one same-attribute feature.
                expected = vec![9, 200, 200, 10, 200, 0, 9, 0, 0, 10, 200, 0];
            }
            assert_eq!(boundaries.features[0].geometry, expected);
            let ocean = scratch.layers[Layer::Ocean as usize].as_ref().unwrap();
            assert_eq!(ocean.features.len(), 2);
            assert!(
                ocean
                    .features
                    .iter()
                    .all(|feature| feature.geometry == polygon)
            );
        }
    }

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

    /// Records spread across one z14-block partition with a deliberately hot
    /// tile band, plus neighbor-partition records so chunks carry multiple
    /// sections. A leading flush forces a whole-file source as well.
    fn tile(raw: u64) -> TileId {
        TileId::new(raw).unwrap()
    }

    fn push_piece_fixture(writer: &mut sort::SortWriter, partition: usize) {
        let base = sort::partition_start_tile_id(partition);
        let neighbor_base = sort::partition_start_tile_id(partition + 1);
        for i in 0..50u64 {
            let tile_id = base + i * 16 + 3;
            let mut data = vec![0u8; 24];
            data[..8].copy_from_slice(&tile_id.to_le_bytes());
            data[8] = 0xEE;
            writer
                .push(sort::SortRecord {
                    key: sort::make_sort_key(tile(tile_id), 6, 1),
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
                    key: sort::make_sort_key(tile(tile_id), layer, 0),
                    data: data.into_boxed_slice(),
                })
                .unwrap();
            if i % 3 == 0 {
                writer
                    .push(sort::SortRecord {
                        key: sort::make_sort_key(tile(neighbor_base + (i % 16)), 0, 0),
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
            let partition_index =
                sort::partition_from_key(sort::make_sort_key(first_tile, 0, 0)) + 5;
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
                let mut piece_reader =
                    sort::SortPartitionReader::open_slices(&piece.sources, compression).unwrap();
                while let Some(record) = piece_reader.next().unwrap() {
                    let tile_id = sort::tile_id_from_key(record.key).raw();
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
        let z7_key = sort::make_sort_key(z7_tile, 0, 0);
        writer
            .push(sort::SortRecord {
                key: z7_key,
                data: Box::from(z7_key.to_le_bytes().as_slice()),
            })
            .unwrap();
        let z14_tile = TileId::at(14, 0, 0);
        let z14_key = sort::make_sort_key(z14_tile, 0, 0);
        writer
            .push(sort::SortRecord {
                key: z14_key,
                data: Box::from(z14_key.to_le_bytes().as_slice()),
            })
            .unwrap();
        let mut reader = writer.finish().unwrap();
        let partitions = reader.take_partitions();
        let z7_partition = partitions
            .iter()
            .find(|partition| partition.index == sort::partition_from_key(z7_key))
            .unwrap();
        let z14_partition = partitions
            .iter()
            .find(|partition| partition.index == sort::partition_from_key(z14_key))
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
