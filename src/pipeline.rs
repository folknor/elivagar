// Tile generation pipeline orchestrator.
//
// Reads an OSM PBF file and produces a Shortbread-schema PMTiles v3 archive.
// Pipeline:
//   Phase 1+2: Single-pass PBF read — build node/way indices AND process features
//   Phase 3:   External merge sort by Hilbert tile ID
//   Phase 4:   Tile assembly (MVT encode + gzip) + PMTiles write


use crate::geometry::{
    self, ClipRect, MercBbox, Point, BUFFER_FRACTION, close_and_orient_cw, close_and_orient_ccw, merc_bbox,
};
use crate::mlt;

/// Full-tile rectangle in tile coordinates (CW, closed). Buffer = 8 rendered pixels = 128 extent units.
/// Used for interior tiles where the polygon fully covers the tile.
const INTERIOR_TILE_RING: [(i32, i32); 5] =
    [(-128, -128), (4224, -128), (4224, 4224), (-128, 4224), (-128, -128)];

/// Maximum zoom at which boundary polygons skip PBF-phase DP simplification.
/// These zooms defer simplification to the assemble phase where cross-feature
/// shared-edge reconciliation is possible.
const BOUNDARY_NO_SIMP_MAX: u8 = 8;
use crate::multipolygon::{self, MemberWay, WayRole};
use crate::mvt::{self, GeomType, LayerBuilder};
use crate::node_index::{NodeIndex, NodeStore, NodeStoreReader, SortedNodeStore};
use crate::ocean;
use crate::pmtiles_writer::{
    self, PmtilesConfig, PmtilesWriter, TileDataCompression, TileDataFormat,
};
use crate::shortbread::{self, AttrValue, GeomExpect, Layer, LayerMatch, OsmGeomType, Tags};
use smallvec::SmallVec;
use crate::sort::{self, SortRecord, SortWriter};
use crate::way_index::WayIndex;
use crate::wire_format::{encode_attrs_bytes, encode_feature_data_with_attrs, add_feature_to_layer};

use pbfhogg::{BlockType, Element, ElementReader, MemberId, PrimitiveBlock};
use rustc_hash::{FxHashMap, FxHashSet};

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// Pipeline error type. Stringly-typed because no caller inspects variants —
/// errors are only displayed or propagated. An enum would add boilerplate for no benefit.
#[derive(Debug)]
pub struct PipelineError(pub String);

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PipelineError {}

// Intentionally converts to String — no caller inspects .source() programmatically.
impl From<std::io::Error> for PipelineError {
    fn from(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}

impl From<String> for PipelineError {
    fn from(s: String) -> Self {
        Self(s)
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Which pipeline phase to skip to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipTo {
    /// Skip PBF read, reuse PBF chunks, re-run ocean + sort + assemble.
    Ocean,
    /// Skip PBF + ocean, reuse all chunks, re-run sort + assemble.
    Sort,
    /// Skip PBF + ocean + sort setup, reuse existing chunks, run assemble only.
    Assemble,
}

/// Tile payload encoding format stored in PMTiles tile data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TilePayloadFormat {
    Mvt,
    Mlt,
}

/// Tile compression algorithm for MVT payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileCompression {
    Gzip,
    Brotli,
}

/// Configuration for the tile generation pipeline.
///
/// All paths are resolved relative to the current working directory.
/// The `tmp_dir` is created automatically if it does not exist.
pub struct TilegenConfig {
    /// Path to the input OpenStreetMap PBF file.
    pub pbf_path: PathBuf,
    /// Path for the output PMTiles v3 archive.
    pub output_path: PathBuf,
    /// Directory for temporary sort chunks and intermediate files.
    pub tmp_dir: PathBuf,
    /// Minimum zoom level (0-14).
    pub min_zoom: u8,
    /// Maximum zoom level (0-14). Must be >= `min_zoom`.
    pub max_zoom: u8,
    /// Ocean polygon shapefile (`water-polygons-split-3857`). Required for
    /// water fill tiles. When combined with `ocean_simplified_shapefile`,
    /// this is used only for z8+.
    pub ocean_shapefile: Option<PathBuf>,
    /// Simplified ocean shapefile for z0-7 (fewer vertices, faster at low zooms).
    /// When set, `ocean_shapefile` is used only for z8+.
    pub ocean_simplified_shapefile: Option<PathBuf>,
    /// Skip to a later phase, reusing checkpoint data from a previous run.
    pub skip_to: Option<SkipTo>,
    /// Keep tile blob in memory instead of streaming to a temp file.
    /// Faster for small extracts, but uses more RAM at planet scale.
    pub in_memory: bool,
    /// Gzip compression level (0-10). Lower = faster, larger output.
    /// Default: 6. Level 3-4 is noticeably faster with ~5% larger output.
    pub compression_level: u32,
    /// Force the compact in-RAM node store even if the PBF header doesn't
    /// declare `Sort.Type_then_ID`. Useful for PBFs that are sorted in practice
    /// but lack the header flag. Aborts with an error if nodes aren't monotonic.
    pub force_sorted: bool,
    /// Allow unsafe flat node-index operation by bypassing unsorted-size and
    /// flat-index-size safety guardrails. Intended only for expert debugging.
    pub allow_unsafe_flat_index: bool,
    /// Thread budget. Controls the rayon global pool size and pbfhogg decode pool.
    /// Default: `std::thread::available_parallelism()` (logical CPUs).
    pub threads: usize,
    /// Byte budget for in-flight way processing.
    /// 0 = mode-aware default:
    /// - 128 MB (standard node-store path)
    /// - 256 MB (`locations_on_ways` mode)
    ///
    /// Controls memory during the PBF way phase. Lower values reduce peak RSS
    /// at the cost of less parallelism.
    pub way_inflight_budget: usize,
    /// Byte budget for relation batch accumulation (0 = default 64 MB).
    /// Controls memory during relation processing. Flush triggers when either
    /// the count limit or byte budget is reached.
    pub rel_batch_budget: usize,
    /// Byte budget for assemble tile batches (0 = default 32 MB).
    /// Controls memory during tile assembly. Dense urban tiles at z14 can
    /// make fixed-count batches very large.
    pub assemble_batch_budget: usize,
    /// Byte budget per sort chunk (0 = default 1 GB).
    /// Records buffer in memory up to this limit, then flush as a sorted
    /// chunk file to disk. Lower values reduce peak RSS during PBF processing
    /// at the cost of more chunk files in the merge phase.
    pub sort_chunk_size: usize,
    /// Use pre-resolved node coordinates from way elements instead of building
    /// a node store. Requires a PBF produced by `pbfhogg add-locations-to-ways`.
    /// Auto-detected from the PBF header's `LocationsOnWays` optional feature
    /// when not set explicitly.
    pub locations_on_ways: bool,
    /// Tile payload format (`mvt` default, `mlt` planned).
    pub tile_format: TilePayloadFormat,
    /// Tile compression algorithm for MVT payloads (`gzip` default, `brotli` optional).
    pub tile_compression: TileCompression,
}

const CHECKPOINT_FILE: &str = "checkpoint.txt";
const SORT_CHECKPOINT_FILE: &str = "sort_chunks.count";
const LAND_MASK_FILE: &str = "land_mask.bin";
const SORT_CHUNKS_DIR: &str = "sort_chunks";
const LON_E7_FULL_CIRCLE: i64 = 3_600_000_000;
/// Default memory budget per sort chunk (1 GB).
const DEFAULT_SORT_CHUNK_SIZE: usize = 1 << 30;
/// Default way in-flight budget for the standard node-store path.
const DEFAULT_WAY_BUDGET: usize = 128 * 1024 * 1024; // 128 MB
/// Default way in-flight budget for locations-on-ways mode.
const DEFAULT_WAY_BUDGET_LOCATIONS: usize = 256 * 1024 * 1024; // 256 MB
/// Reject unsorted flat-index path above this input size unless explicitly overridden.
const MAX_FLAT_PBF_SIZE: u64 = 1024 * 1024 * 1024; // 1 GB
const TILE_OVERSIZE_WARN_BYTES: u64 = 500 * 1024;
const TILE_OVERSIZE_SEVERE_BYTES: u64 = 1024 * 1024;
const TILE_OVERSIZE_TOP_N: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeStoreMode {
    None,
    Sorted,
    Flat { unsafe_override: bool },
}

fn unsorted_flat_guard_error(pbf_size: u64) -> PipelineError {
    PipelineError(format!(
        "PBF file is {:.1} GB but does not declare Sort.Type_then_ID.\n\
         The flat node index would create a ~96 GB sparse file, causing severe\n\
         performance degradation on machines with <128 GB RAM. Options:\n\
         1. Use --force-sorted if the PBF is actually sorted (most Geofabrik extracts are)\n\
         2. Sort the PBF first with: pbfhogg sort input.pbf -o sorted.pbf\n\
         3. Alternative sorter: osmium sort input.pbf -o sorted.pbf\n\
         4. Use a sorted PBF from Geofabrik or planet.openstreetmap.org",
        pbf_size as f64 / (1024.0 * 1024.0 * 1024.0),
    ))
}

fn select_node_store_mode(
    locations_on_ways: bool,
    header_sorted: bool,
    force_sorted: bool,
    pbf_size: u64,
    allow_unsafe_flat_index: bool,
) -> Result<NodeStoreMode, PipelineError> {
    if locations_on_ways {
        return Ok(NodeStoreMode::None);
    }
    if header_sorted || force_sorted {
        return Ok(NodeStoreMode::Sorted);
    }
    if pbf_size > MAX_FLAT_PBF_SIZE && !allow_unsafe_flat_index {
        return Err(unsorted_flat_guard_error(pbf_size));
    }
    Ok(NodeStoreMode::Flat {
        unsafe_override: allow_unsafe_flat_index,
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct MissingRefStats {
    missing_way_node_refs: u64,
    ways_with_missing_node_refs: u64,
    missing_relation_way_refs: u64,
    relations_with_missing_way_refs: u64,
    relation_non_way_members: u64,
    relation_nested_members: u64,
}

#[derive(Debug, Default)]
struct MissingRefStatsAtomic {
    missing_way_node_refs: AtomicU64,
    ways_with_missing_node_refs: AtomicU64,
    missing_relation_way_refs: AtomicU64,
    relations_with_missing_way_refs: AtomicU64,
    relation_non_way_members: AtomicU64,
    relation_nested_members: AtomicU64,
}

impl MissingRefStatsAtomic {
    fn record_way_missing_nodes(&self, missing_refs: usize) {
        self.missing_way_node_refs
            .fetch_add(missing_refs as u64, Ordering::Relaxed);
        self.ways_with_missing_node_refs.fetch_add(1, Ordering::Relaxed);
    }

    fn record_relation_missing_way_ref(&self) {
        self.missing_relation_way_refs.fetch_add(1, Ordering::Relaxed);
    }

    fn record_relation_with_missing_way_refs(&self) {
        self.relations_with_missing_way_refs
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_relation_non_way_member(&self) {
        self.relation_non_way_members.fetch_add(1, Ordering::Relaxed);
    }

    fn record_relation_nested_member(&self) {
        self.relation_nested_members.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> MissingRefStats {
        MissingRefStats {
            missing_way_node_refs: self.missing_way_node_refs.load(Ordering::Relaxed),
            ways_with_missing_node_refs: self.ways_with_missing_node_refs.load(Ordering::Relaxed),
            missing_relation_way_refs: self.missing_relation_way_refs.load(Ordering::Relaxed),
            relations_with_missing_way_refs: self
                .relations_with_missing_way_refs
                .load(Ordering::Relaxed),
            relation_non_way_members: self.relation_non_way_members.load(Ordering::Relaxed),
            relation_nested_members: self.relation_nested_members.load(Ordering::Relaxed),
        }
    }
}

fn missing_ref_summary_lines(summary: MissingRefStats) -> [String; 6] {
    [
        format!("missing_way_node_refs={}", summary.missing_way_node_refs),
        format!(
            "ways_with_missing_node_refs={}",
            summary.ways_with_missing_node_refs
        ),
        format!(
            "missing_relation_way_refs={}",
            summary.missing_relation_way_refs
        ),
        format!(
            "relations_with_missing_way_refs={}",
            summary.relations_with_missing_way_refs
        ),
        format!("relation_non_way_members={}", summary.relation_non_way_members),
        format!("relation_nested_members={}", summary.relation_nested_members),
    ]
}

#[derive(Clone, Copy, Debug, Default)]
struct OversizeTile {
    tile_id: u64,
    bytes: u64,
}

#[derive(Debug, Default)]
struct TileSizeDiagnostics {
    total_tile_bytes: u64,
    max_tile: OversizeTile,
    oversize_warn_count: u64,
    oversize_severe_count: u64,
    top_oversized: [OversizeTile; TILE_OVERSIZE_TOP_N],
}

fn insert_top_oversized(top: &mut [OversizeTile; TILE_OVERSIZE_TOP_N], tile: OversizeTile) {
    let mut pos = None;
    for (i, t) in top.iter().enumerate() {
        if tile.bytes > t.bytes {
            pos = Some(i);
            break;
        }
    }
    let Some(i) = pos else { return };
    for j in (i + 1..top.len()).rev() {
        top[j] = top[j - 1];
    }
    top[i] = tile;
}

fn record_tile_size_diagnostics(size_diag: &mut TileSizeDiagnostics, tile_id: u64, tile_bytes: u64) {
    size_diag.total_tile_bytes += tile_bytes;
    if tile_bytes > size_diag.max_tile.bytes {
        size_diag.max_tile = OversizeTile {
            tile_id,
            bytes: tile_bytes,
        };
    }
    if tile_bytes > TILE_OVERSIZE_WARN_BYTES {
        size_diag.oversize_warn_count += 1;
    }
    if tile_bytes > TILE_OVERSIZE_SEVERE_BYTES {
        size_diag.oversize_severe_count += 1;
    }
    insert_top_oversized(
        &mut size_diag.top_oversized,
        OversizeTile {
            tile_id,
            bytes: tile_bytes,
        },
    );
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the full tile generation pipeline.
///
/// Reads the PBF file, processes ocean shapefiles (if provided), sorts all
/// feature records by Hilbert tile ID, and writes the output PMTiles archive.
///
/// Read peak resident set size (VmHWM) from `/proc/self/status`.
/// Returns `None` on non-Linux platforms or if parsing fails.
///
/// NOTE: VmHWM is the process-lifetime high-water mark — it never decreases.
/// Per-phase values (phase12_rss_kb, ocean_rss_kb, etc.) are therefore
/// monotonically non-decreasing. This is intentional: each value shows the
/// cumulative peak RSS up to that phase, not the phase's isolated contribution.
fn peak_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.trim().strip_suffix("kB")?.trim().parse().ok();
        }
    }
    None
}

/// Read current resident set size (VmRSS) from `/proc/self/status`.
/// Returns `None` on non-Linux platforms or if parsing fails.
fn current_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.trim().strip_suffix("kB")?.trim().parse().ok();
        }
    }
    None
}

/// # Errors
///
/// Returns [`PipelineError`] on I/O failures, invalid configuration (e.g.
/// `max_zoom > 14`), or corrupt input data.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
#[hotpath::measure]
pub fn run(config: &TilegenConfig) -> Result<(), PipelineError> {
    if config.max_zoom > 14 {
        return Err(PipelineError(format!(
            "max_zoom {} exceeds maximum supported zoom level 14",
            config.max_zoom
        )));
    }
    if config.min_zoom > config.max_zoom {
        return Err(PipelineError(format!(
            "min_zoom {} is greater than max_zoom {}",
            config.min_zoom, config.max_zoom
        )));
    }

    let sort_chunk_size = if config.sort_chunk_size > 0 {
        config.sort_chunk_size
    } else {
        DEFAULT_SORT_CHUNK_SIZE
    };

    let total_start = Instant::now();
    let skip = config.skip_to;

    eprintln!("=== Tilegen: {} → {}", config.pbf_path.display(), config.output_path.display());
    eprintln!("    Zoom range: z{}–z{}", config.min_zoom, config.max_zoom);
    eprintln!("    Tmp dir:    {}", config.tmp_dir.display());
    eprintln!("    Tile format:{:?}, compression:{:?}", config.tile_format, config.tile_compression);
    if let Some(s) = skip {
        eprintln!("    Skip to:    {s:?}");
    }

    // --- Phase 1+2: PBF read + feature processing ---
    let phase12_elapsed;
    let ocean_elapsed;
    let mut phase12_rss: Option<u64> = None;
    let mut ocean_rss: Option<u64> = None;
    let mut max_way_inflight_bytes: Option<usize> = None;
    let mut max_rel_batch_bytes: Option<usize> = None;
    let mut relation_blocks_buffered: Option<usize> = None;
    let mut relation_blocks_drop_rss_kb: Option<u64> = None;

    let mut node_store_stats: Option<(u64, usize)> = None;
    let mut missing_ref_summary: Option<MissingRefStats> = None;

    let mut sort_writer = if matches!(skip, Some(SkipTo::Sort | SkipTo::Assemble)) {
        // Skip straight to later phases — reuse existing chunks on disk.
        phase12_elapsed = None;
        ocean_elapsed = None;
        if skip == Some(SkipTo::Sort) {
            eprintln!("--- Skipping to sort (using existing chunks) ---");
        } else {
            eprintln!("--- Skipping to assemble (using existing chunks) ---");
        }
        None
    } else {
        let (mut sort_writer, land_mask) = if skip.is_none() {
            // Full run: clean tmp dir and run PBF phase
            drop(std::fs::remove_dir_all(&config.tmp_dir)); // Best-effort: may not exist yet.
            std::fs::create_dir_all(&config.tmp_dir)?; // io::Error message is sufficient context.

            let phase12_start = Instant::now();
            let (mut sw, bounds_out, mask, ns_stats, way_hwm, rel_hwm, rel_blocks, rel_drop_rss, missing_refs) = phase_read_and_process(config)?;
            node_store_stats = ns_stats;
            max_way_inflight_bytes = Some(way_hwm);
            max_rel_batch_bytes = Some(rel_hwm);
            relation_blocks_buffered = Some(rel_blocks);
            relation_blocks_drop_rss_kb = rel_drop_rss;
            missing_ref_summary = Some(missing_refs);
            phase12_elapsed = Some(phase12_start.elapsed());
            phase12_rss = peak_rss_kb();
            sw.flush()?; // Flush buffer so chunk_count() is accurate for checkpoint
            save_checkpoint(&config.tmp_dir, &bounds_out, sw.chunk_count())?;
            save_land_mask(&config.tmp_dir, &mask)?;
            (sw, Some(mask))
        } else {
            // --skip-to ocean: load checkpoint, resume from PBF chunks
            let (_, pbf_chunks) = load_checkpoint(&config.tmp_dir)?;
            eprintln!("--- Skipping PBF phase ({pbf_chunks} chunks from checkpoint) ---");
            phase12_elapsed = None;
            let sw = sort::SortWriter::resume(&config.tmp_dir.join(SORT_CHUNKS_DIR), sort_chunk_size, pbf_chunks)?;
            let mask = load_land_mask(&config.tmp_dir);
            if mask.is_none() {
                eprintln!("  No land mask found — ocean filtering disabled");
            }
            (sw, mask)
        };

        // Load data_bounds (needed for ocean, always available from checkpoint or just computed)
        let (data_bounds, _) = load_checkpoint(&config.tmp_dir)?;
        let mask_ref = land_mask.as_ref();

        // --- Ocean shapefile processing ---
        // When a simplified shapefile is provided, use it for z0-7 and the
        // full-resolution shapefile for z8+. Otherwise use the full-res for all zooms.
        ocean_elapsed = if let Some(ref ocean_path) = config.ocean_shapefile {
            let ocean_start = Instant::now();
            eprintln!("--- Ocean shapefile ---");
            let mut ocean_features: u64 = 0;

            if let Some(ref simplified_path) = config.ocean_simplified_shapefile {
                let simplified_max = config.max_zoom.min(7);
                if config.min_zoom <= simplified_max {
                    eprintln!("  Simplified (z{}–z{}):", config.min_zoom, simplified_max);
                    ocean_features += ocean::process_ocean_shapefile(
                        simplified_path, &data_bounds, config.min_zoom, simplified_max, mask_ref, &mut sort_writer,
                    )?;
                }
                if config.max_zoom >= 8 {
                    let full_min = config.min_zoom.max(8);
                    eprintln!("  Full-resolution (z{full_min}–z{}):", config.max_zoom);
                    ocean_features += ocean::process_ocean_shapefile(
                        ocean_path, &data_bounds, full_min, config.max_zoom, mask_ref, &mut sort_writer,
                    )?;
                }
            } else {
                ocean_features = ocean::process_ocean_shapefile(
                    ocean_path, &data_bounds, config.min_zoom, config.max_zoom, mask_ref, &mut sort_writer,
                )?;
            }

            let elapsed = ocean_start.elapsed();
            eprintln!("  {ocean_features} features in {elapsed:.2?}");
            ocean_rss = peak_rss_kb();
            Some((elapsed, ocean_features))
        } else {
            None
        };

        Some(sort_writer)
    };

    // --- Phase 3: Sort ---
    // Flush any trailing buffer so chunk_count() reflects all chunks on disk,
    // then save the count for --skip-to sort validation.
    if let Some(ref mut sw) = sort_writer {
        sw.flush()?;
    }
    let sort_chunks = sort_writer.as_ref().map(sort::SortWriter::chunk_count);
    save_sort_chunk_count(&config.tmp_dir, sort_chunks)?;
    let (mut sort_reader, phase3_elapsed, sort_rss) = if skip == Some(SkipTo::Assemble) {
        let sr = sort::SortReader::from_dir(
            &config.tmp_dir.join(SORT_CHUNKS_DIR),
            load_sort_chunk_count(&config.tmp_dir),
        )?;
        (sr, None, peak_rss_kb())
    } else {
        let phase3_start = Instant::now();
        eprintln!("--- Sort ---");
        let sr = if let Some(sw) = sort_writer {
            sw.finish()?
        } else {
            sort::SortReader::from_dir(
                &config.tmp_dir.join(SORT_CHUNKS_DIR),
                load_sort_chunk_count(&config.tmp_dir),
            )?
        };
        (sr, Some(phase3_start.elapsed()), peak_rss_kb())
    };

    // --- Phase 4: Tile assembly + PMTiles write ---
    let phase4_start = Instant::now();
    eprintln!("--- Tile assembly ---");
    let (features_read, tiles_written, unique_tiles, max_assemble_batch_bytes, dedup_stats, tile_size_diag) =
        phase_assemble(&mut sort_reader, config)?;
    let phase4_elapsed = phase4_start.elapsed();
    let assemble_rss = peak_rss_kb();

    let total = total_start.elapsed();

    // Machine-readable summary (all times in milliseconds)
    eprintln!("---");
    eprintln!("total_ms={}", total.as_millis());
    if let Some(p12) = phase12_elapsed {
        eprintln!("phase12_ms={}", p12.as_millis());
    }
    if let Some((oe, of)) = ocean_elapsed {
        eprintln!("ocean_ms={}", oe.as_millis());
        eprintln!("ocean_features={of}");
    }
    if let Some(p3) = phase3_elapsed {
        eprintln!("phase3_ms={}", p3.as_millis());
    }
    eprintln!("phase4_ms={}", phase4_elapsed.as_millis());
    eprintln!("features={features_read}");
    eprintln!("tiles={tiles_written}");
    eprintln!("unique_tiles={unique_tiles}");
    eprintln!(
        "tile_format={}",
        match config.tile_format {
            TilePayloadFormat::Mvt => "mvt",
            TilePayloadFormat::Mlt => "mlt",
        }
    );
    eprintln!(
        "tile_compression={}",
        match config.tile_compression {
            TileCompression::Gzip => "gzip",
            TileCompression::Brotli => "brotli",
        }
    );
    if let Ok(meta) = std::fs::metadata(&config.output_path) {
        eprintln!("output_bytes={}", meta.len());
    }
    if let Some((nodes, groups)) = node_store_stats {
        eprintln!("node_store_nodes={nodes}");
        eprintln!("node_store_groups={groups}");
    }
    if let Some(n) = sort_chunks {
        eprintln!("sort_chunks={n}");
    }
    if let Some(kb) = phase12_rss {
        eprintln!("phase12_rss_kb={kb}");
    }
    if let Some(kb) = ocean_rss {
        eprintln!("ocean_rss_kb={kb}");
    }
    if let Some(kb) = sort_rss {
        eprintln!("sort_rss_kb={kb}");
    }
    if let Some(kb) = assemble_rss {
        eprintln!("assemble_rss_kb={kb}");
    }
    let peak_rss = [phase12_rss, ocean_rss, sort_rss, assemble_rss]
        .iter()
        .filter_map(|v| *v)
        .max();
    if let Some(kb) = peak_rss {
        eprintln!("peak_rss_kb={kb}");
    }
    if let Some(bytes) = max_way_inflight_bytes {
        eprintln!("max_way_inflight_bytes={bytes}");
    }
    if let Some(bytes) = max_rel_batch_bytes {
        eprintln!("max_rel_batch_bytes={bytes}");
    }
    if let Some(n) = relation_blocks_buffered {
        eprintln!("relation_blocks_buffered={n}");
    }
    if let Some(kb) = relation_blocks_drop_rss_kb {
        eprintln!("relation_blocks_drop_rss_kb={kb}");
    }
    eprintln!("max_assemble_batch_bytes={max_assemble_batch_bytes}");
    eprintln!("dedup_candidates={}", dedup_stats.candidates);
    eprintln!("dedup_tiles_reused={}", dedup_stats.tiles_reused);
    eprintln!("dedup_bytes_saved={}", dedup_stats.bytes_saved);
    eprintln!("dedup_reject_len_mismatch={}", dedup_stats.reject_len_mismatch);
    eprintln!("dedup_reject_fp_mismatch={}", dedup_stats.reject_fp_mismatch);
    eprintln!("dedup_insert_skipped_cap={}", dedup_stats.insert_skipped_cap);
    eprintln!("dedup_hash_bucket_collisions={}", dedup_stats.hash_bucket_collisions);
    eprintln!("tile_bytes_total={}", tile_size_diag.total_tile_bytes);
    eprintln!(
        "tile_bytes_avg={}",
        tile_size_diag
            .total_tile_bytes
            .checked_div(tiles_written)
            .unwrap_or(0)
    );
    eprintln!("tile_max_bytes={}", tile_size_diag.max_tile.bytes);
    if tile_size_diag.max_tile.bytes > 0 {
        let (z, x, y) = pmtiles_writer::tile_id_to_zxy(tile_size_diag.max_tile.tile_id);
        eprintln!("tile_max_zxy={z}/{x}/{y}");
    }
    eprintln!("oversize_tiles_warn={}", tile_size_diag.oversize_warn_count);
    eprintln!("oversize_tiles_severe={}", tile_size_diag.oversize_severe_count);
    for (i, t) in tile_size_diag.top_oversized.iter().enumerate() {
        if t.bytes == 0 {
            continue;
        }
        let (z, x, y) = pmtiles_writer::tile_id_to_zxy(t.tile_id);
        eprintln!("oversize_top_{}={z}/{x}/{y}:{}", i + 1, t.bytes);
    }
    if let Some(m) = missing_ref_summary {
        for line in missing_ref_summary_lines(m) {
            eprintln!("{line}");
        }
    }
    Ok(())
}

fn save_checkpoint(tmp_dir: &std::path::Path, bounds: &MercBbox, chunk_count: usize) -> Result<(), PipelineError> {
    let path = tmp_dir.join(CHECKPOINT_FILE);
    let content = format!(
        "{} {} {} {} {}",
        bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y, chunk_count
    );
    std::fs::write(path, content)?; // io::Error message is sufficient context.
    Ok(())
}

/// Save total chunk count so --skip-to sort can validate against stale leftovers.
fn save_sort_chunk_count(tmp_dir: &std::path::Path, count: Option<usize>) -> Result<(), PipelineError> {
    if let Some(n) = count {
        std::fs::write(tmp_dir.join(SORT_CHECKPOINT_FILE), n.to_string())?;
    }
    Ok(())
}

fn load_sort_chunk_count(tmp_dir: &std::path::Path) -> Option<usize> {
    match std::fs::read_to_string(tmp_dir.join(SORT_CHECKPOINT_FILE)) {
        Ok(content) => content.trim().parse().ok(),
        Err(_) => {
            eprintln!("  Warning: no sort checkpoint found — cannot verify chunk integrity");
            None
        }
    }
}

fn save_land_mask(tmp_dir: &std::path::Path, mask: &geometry::LandMask) -> Result<(), PipelineError> {
    std::fs::write(tmp_dir.join(LAND_MASK_FILE), mask.to_bytes())?;
    Ok(())
}

fn load_land_mask(tmp_dir: &std::path::Path) -> Option<geometry::LandMask> {
    let data = std::fs::read(tmp_dir.join(LAND_MASK_FILE)).ok()?;
    geometry::LandMask::from_bytes(&data)
}

fn load_checkpoint(tmp_dir: &std::path::Path) -> Result<(MercBbox, usize), PipelineError> {
    let path = tmp_dir.join(CHECKPOINT_FILE);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| PipelineError(format!("no checkpoint in {}: {e} (run a full tilegen first)", tmp_dir.display())))?;
    let parts: Vec<&str> = content.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(PipelineError(format!("invalid checkpoint format: expected 5 fields, got {}", parts.len())));
    }
    let parse = |s: &str, name: &str| -> Result<f64, PipelineError> {
        s.parse().map_err(|e| PipelineError(format!("checkpoint parse {name}: {e}")))
    };
    let bounds = MercBbox {
        min_x: parse(parts[0], "min_x")?,
        min_y: parse(parts[1], "min_y")?,
        max_x: parse(parts[2], "max_x")?,
        max_y: parse(parts[3], "max_y")?,
    };
    let chunks: usize = parts[4].parse()
        .map_err(|e| PipelineError(format!("checkpoint parse chunk count: {e}")))?;
    Ok((bounds, chunks))
}

// ---------------------------------------------------------------------------
// Phase 1+2: Single-pass PBF read + feature processing
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines, clippy::cognitive_complexity, clippy::unwrap_in_result, clippy::type_complexity)]
#[hotpath::measure]
fn phase_read_and_process(config: &TilegenConfig) -> Result<(SortWriter, MercBbox, geometry::LandMask, Option<(u64, usize)>, usize, usize, usize, Option<u64>, MissingRefStats), PipelineError> {
    eprintln!("\n--- Phase 1+2: Reading PBF + processing features ---");

    let sort_chunk_budget = if config.sort_chunk_size > 0 {
        config.sort_chunk_size
    } else {
        DEFAULT_SORT_CHUNK_SIZE
    };

    // Option so we can move to drain thread during way phase and get back after.
    let mut sort_writer: Option<SortWriter> = Some(
        SortWriter::new(&config.tmp_dir.join(SORT_CHUNKS_DIR), sort_chunk_budget)?
    );

    // Decode threads: give 1/3 of budget to pbfhogg decode, rest to rayon processing.
    let decode_threads = (config.threads / 3).max(1);
    let reader =
        ElementReader::from_path(&config.pbf_path)
            .map_err(|e| PipelineError(format!("failed to open PBF: {e}")))?
            .decode_threads(decode_threads);

    let idx_dir = &config.tmp_dir;
    // Option so we can consume it via .take() on first Way element.
    let locations_on_ways = config.locations_on_ways
        || reader.header().optional_features().iter().any(|f| f == "LocationsOnWays");

    let pbf_size = std::fs::metadata(&config.pbf_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let node_store_mode = select_node_store_mode(
        locations_on_ways,
        reader.header().is_sorted(),
        config.force_sorted,
        pbf_size,
        config.allow_unsafe_flat_index,
    )?;
    let mut node_store_opt: Option<NodeStore> = match node_store_mode {
        NodeStoreMode::None => {
            eprintln!("  LocationsOnWays — skipping node store");
            None
        }
        NodeStoreMode::Sorted => {
            if config.force_sorted && !reader.header().is_sorted() {
                eprintln!("  --force-sorted: assuming sorted PBF (will abort if not)");
            } else {
                eprintln!("  PBF declares Sort.Type_then_ID — using compact node store");
            }
            Some(NodeStore::Sorted(SortedNodeStore::new()))
        }
        NodeStoreMode::Flat { unsafe_override } => {
            if unsafe_override {
                eprintln!(
                    "  WARNING: unsafe flat index override enabled; bypassing unsorted-size and flat-index-size safety guardrails"
                );
            }
            eprintln!("  PBF not sorted — using flat mmap node index");
            Some(NodeStore::Flat(if unsafe_override {
                NodeIndex::create_unbounded(&idx_dir.join("nodes.idx"))?
            } else {
                NodeIndex::create(&idx_dir.join("nodes.idx"))?
            }))
        }
    };
    // Option so we can move to drain thread during way phase and get back after.
    let mut way_index: Option<WayIndex> = Some(WayIndex::create(idx_dir)?);

    let mut node_count: u64 = 0;
    let mut way_count: u64 = 0;
    let mut rel_count: u64 = 0;
    let mut features_emitted: u64 = 0;
    let mut node_store_stats: Option<(u64, usize)> = None;
    let missing_ref_stats = std::sync::Arc::new(MissingRefStatsAtomic::default());
    let land_mask = std::sync::Arc::new(geometry::LandMask::new());

    // Track data extent for ocean shapefile filtering
    let mut min_lat_e7: i32 = i32::MAX;
    let mut max_lat_e7: i32 = i32::MIN;
    let mut min_lon_e7: i32 = i32::MAX;
    let mut max_lon_e7: i32 = i32::MIN;
    let mut min_lon_shifted_e7: i64 = i64::MAX;
    let mut max_lon_shifted_e7: i64 = i64::MIN;

    let min_z = config.min_zoom;
    let max_z = config.max_zoom;

    let mut rel_batch: Vec<PreparedRelation> = Vec::with_capacity(REL_BATCH_SIZE);
    let rel_budget = if config.rel_batch_budget > 0 {
        config.rel_batch_budget
    } else {
        REL_BATCH_BUDGET_DEFAULT
    };

    // Block-level dispatch: worker thread receives entire PrimitiveBlocks containing
    // ways, extracts RawWay data and processes via rayon. Main thread sends blocks
    // and drains results — no per-way work on the main thread during the way phase.
    let mut block_tx: Option<std::sync::mpsc::SyncSender<PrimitiveBlock>> = None;
    let mut worker_handle: Option<std::thread::JoinHandle<()>> = None;
    // Drain thread owns way_index + sort_writer during way phase, returns them when done.
    let mut drain_handle: Option<std::thread::JoinHandle<(WayIndex, SortWriter, u64)>> = None;

    // Buffer relation blocks — processed after all PBF blocks are consumed so that
    // late way blocks (common in locations-on-ways PBFs) don't hit a finalized way_index.
    let mut relation_blocks: Vec<PrimitiveBlock> = Vec::new();

    // High-water-mark counters for in-flight memory tracking.
    let way_hwm = std::sync::Arc::new(AtomicUsize::new(0));
    let mut rel_batch_bytes: usize = 0;
    let mut max_rel_batch_bytes: usize = 0;

    // Reusable buffer hoisted out of the PBF closure to avoid per-element
    // allocation (~200M allocs at planet scale). Cleared each iteration.
    // tags_vec cannot be hoisted: it holds &str references into PBF elements
    // that don't outlive the closure body (mutable reference invariance).
    let mut node_records: Vec<SortRecord> = Vec::new();

    // Macro to handle Node and DenseNode identically — both types expose the
    // same API (.id(), .decimicro_lat(), .decimicro_lon(), .tags()) but are
    // distinct types, so a generic function would not work without a trait.
    macro_rules! handle_node {
        ($node:expr) => {{
            node_count += 1;
            let lat_e7 = $node.decimicro_lat();
            let lon_e7 = $node.decimicro_lon();
            if let Some(ns) = node_store_opt.as_mut() {
                ns.put($node.id(), lat_e7, lon_e7);
            }

            min_lat_e7 = min_lat_e7.min(lat_e7);
            max_lat_e7 = max_lat_e7.max(lat_e7);
            min_lon_e7 = min_lon_e7.min(lon_e7);
            max_lon_e7 = max_lon_e7.max(lon_e7);
            let shifted_lon_e7 = lon_e7_shifted_360(lon_e7);
            min_lon_shifted_e7 = min_lon_shifted_e7.min(shifted_lon_e7);
            max_lon_shifted_e7 = max_lon_shifted_e7.max(shifted_lon_e7);

            if $node.tags().next().is_some() {
                let tags_vec: Vec<(&str, &str)> = $node.tags().collect();
                node_records.clear();
                #[allow(clippy::cast_sign_loss)]
                let n = process_node(
                    $node.id() as u64, lat_e7, lon_e7,
                    &tags_vec, min_z, max_z, &land_mask, &mut node_records,
                );
                // Panic: inside PBF callback — can't propagate Result. Disk I/O failure is unrecoverable.
                for r in node_records.drain(..) {
                    sort_writer.as_mut().expect("sort_writer taken by drain thread")
                        .push(r).expect("sort push failed");
                }
                features_emitted += n;
            }
        }};
    }

    for block_result in reader.into_blocks_pipelined() {
        let block = block_result
            .map_err(|e| PipelineError(format!("PBF read failed: {e}")))?;

        // Classify block by reading first wire tag byte per group —
        // no element decoding. Sorted PBFs have single-type blocks.
        match block.block_type() {
            BlockType::DenseNodes | BlockType::Nodes => {
                // Node block — process inline
                block.for_each_element(|element| match element {
                    Element::DenseNode(node) => handle_node!(node),
                    Element::Node(node) => handle_node!(node),
                    _ => {}
                });
            }
            BlockType::Ways => {
                // Way block — send entire block to worker thread.
                // Count ways from block (elements() re-parses from bytes, cheap).
                way_count += block.elements()
                    .filter(|e| matches!(e, Element::Way(_)))
                    .count() as u64;

                // Spawn worker + drain threads on first way block
                if block_tx.is_none() {
                    let nr: Option<std::sync::Arc<NodeStoreReader>> = if locations_on_ways {
                        None
                    } else {
                        let ns = node_store_opt.take()
                            .expect("node store already consumed");
                        let r = std::sync::Arc::new(
                            ns.into_reader().expect("failed to convert node store to reader")
                        );
                        node_store_stats = r.sorted_stats();
                        Some(r)
                    };
                    if locations_on_ways {
                        eprintln!("  LocationsOnWays mode — processing ways (no node store)...");
                    } else {
                        eprintln!("  Node store finalized ({node_count} nodes), processing ways...");
                    }

                    let (btx, brx) = std::sync::mpsc::sync_channel::<PrimitiveBlock>(1);
                    // Capacity must be >= MAX_INFLIGHT: rayon tasks block on send()
                    // while holding a rayon thread. If capacity < inflight tasks,
                    // blocked senders tie up all rayon threads → worker (which runs
                    // inside rayon::in_place_scope) can't make progress → deadlock.
                    let (rtx, rrx) = std::sync::mpsc::sync_channel::<Vec<ProcessedWay>>(MAX_INFLIGHT);
                    let nr_clone = nr.clone();
                    let lm_clone = std::sync::Arc::clone(&land_mask);
                    let way_hwm_clone = std::sync::Arc::clone(&way_hwm);
                    let missing_ref_stats_clone = std::sync::Arc::clone(&missing_ref_stats);
                    let mz = min_z;
                    let xz = max_z;
                    // Multi-block overlap: rayon::scope allows multiple blocks' ways
                    // in the pool simultaneously. Byte-budgeted in-flight control
                    // limits total estimated memory, with a count ceiling as safety net.
                    const MAX_INFLIGHT: usize = 8;
                    const WAY_OUTPUT_MULTIPLIER: usize = 10;
                    let way_budget = if config.way_inflight_budget > 0 {
                        config.way_inflight_budget
                    } else if locations_on_ways {
                        DEFAULT_WAY_BUDGET_LOCATIONS
                    } else {
                        DEFAULT_WAY_BUDGET
                    };
                    worker_handle = Some(std::thread::spawn(move || {
                        use rayon::prelude::*;
                        // Take refs outside loop — Copy into each move closure,
                        // avoids Arc::clone per spawn.
                        let nr_ref: Option<&NodeStoreReader> = nr_clone.as_deref();
                        let lm_ref = &*lm_clone;
                        let mr_ref = &*missing_ref_stats_clone;
                        // Byte-budgeted throttle: (count, estimated_bytes).
                        // Condvar wakes dispatcher when a task completes.
                        let inflight = std::sync::Mutex::new((0usize, 0usize));
                        let inflight_cvar = std::sync::Condvar::new();
                        let inflight_ref = &inflight;
                        let cvar_ref = &inflight_cvar;
                        rayon::in_place_scope(|s| {
                            while let Ok(block) = brx.recv() {
                                let raw_ways: Vec<RawWay> = block.elements()
                                    .filter_map(|e| match e {
                                        Element::Way(way) => {
                                            let tags: Vec<(String, String)> = way.tags()
                                                .map(|(k, v)| (k.to_string(), v.to_string()))
                                                .collect();
                                            if nr_ref.is_some() {
                                                // Standard PBF: collect node refs
                                                let node_refs: Vec<i64> = way.refs().collect();
                                                if node_refs.is_empty() { return None; }
                                                Some(RawWay {
                                                    way_id: way.id(),
                                                    node_refs,
                                                    preserve_node_refs: Vec::new(),
                                                    coords_e7: Vec::new(),
                                                    tags,
                                                })
                                            } else {
                                                // Locations-on-ways: collect refs + coords directly
                                                let node_refs: Vec<i64> = way.refs().collect();
                                                let coords_e7: Vec<(i32, i32)> = way
                                                    .node_locations()
                                                    .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon()))
                                                    .collect();
                                                if coords_e7.is_empty() { return None; }
                                                Some(RawWay {
                                                    way_id: way.id(),
                                                    node_refs,
                                                    preserve_node_refs: Vec::new(),
                                                    coords_e7,
                                                    tags,
                                                })
                                            }
                                        }
                                        _ => None,
                                    })
                                    .collect();
                                let mut raw_ways = raw_ways;
                                annotate_block_shared_node_refs(&mut raw_ways);
                                let block_bytes = estimate_raw_ways_bytes(&raw_ways);
                                let block_cost = block_bytes * WAY_OUTPUT_MULTIPLIER;
                                // Wait for capacity: count limit and byte budget.
                                // Always allow at least one task — a single block that
                                // exceeds the byte budget must not deadlock the condvar
                                // (no in-flight tasks → no notify_one → permanent sleep).
                                {
                                    let mut guard = inflight_ref.lock()
                                        .expect("inflight lock");
                                    guard = inflight_cvar.wait_while(guard, |&mut (count, bytes)| {
                                        count >= MAX_INFLIGHT
                                            || (count > 0 && bytes + block_cost > way_budget)
                                    }).expect("condvar wait");
                                    guard.0 += 1;
                                    guard.1 += block_cost;
                                    // Update HWM with current in-flight bytes (raw, not multiplied).
                                    way_hwm_clone.fetch_max(
                                        guard.1 / WAY_OUTPUT_MULTIPLIER, Ordering::Relaxed,
                                    );
                                }
                                let tx = rtx.clone();
                                #[allow(clippy::let_underscore_must_use)]
                                s.spawn(move |_| {
                                    let results: Vec<ProcessedWay> = raw_ways
                                        .into_par_iter()
                                        .map(|raw| process_raw_way(
                                            &raw, nr_ref, lm_ref, mz, xz, mr_ref,
                                        ))
                                        .collect();
                                    let _ = tx.send(results);
                                    let mut guard = inflight_ref.lock()
                                        .expect("inflight lock");
                                    guard.0 -= 1;
                                    guard.1 -= block_cost;
                                    cvar_ref.notify_one();
                                });
                            }
                        });
                        // Scope waits for all spawned tasks. rtx drops here →
                        // drain's rrx.recv() returns Err → drain exits.
                    }));

                    // Drain thread: owns way_index + sort_writer, writes results as they arrive.
                    // Runs concurrently with worker — main thread is free to forward blocks.
                    let mut wi = way_index.take().expect("way_index already taken");
                    let mut sw = sort_writer.take().expect("sort_writer already taken");
                    drain_handle = Some(std::thread::spawn(move || {
                        let mut count: u64 = 0;
                        while let Ok(results) = rrx.recv() {
                            count += drain_processed_ways(results, &mut wi, &mut sw);
                        }
                        (wi, sw, count)
                    }));

                    block_tx = Some(btx);
                }

                // send() blocks if worker is still processing previous block (backpressure)
                block_tx.as_ref().expect("worker not initialized")
                    .send(block).expect("worker thread panicked");
            }
            BlockType::Relations => {
                // Buffer relation blocks — defer processing until all PBF blocks
                // are consumed. Locations-on-ways PBFs can have way blocks after
                // relation blocks; processing relations inline would finalize the
                // way_index too early.
                relation_blocks.push(block);
            }
            BlockType::Empty | BlockType::Mixed => {}
        }
    }

    // Shut down worker + drain after all PBF blocks consumed.
    if block_tx.is_some() {
        drop(block_tx.take());
        if let Some(h) = worker_handle.take() {
            h.join().expect("worker thread panicked");
        }
        if let Some(h) = drain_handle.take() {
            let (wi, sw, count) = h.join().expect("drain thread panicked");
            way_index = Some(wi);
            sort_writer = Some(sw);
            features_emitted += count;
        }
    }

    // Finalize way index after all way blocks are processed.
    if let Some(ref mut wi) = way_index {
        wi.finish_writing().expect("failed to finalize way index");
    }
    eprintln!("  Ways: {way_count}, Features so far: {features_emitted}");
    eprintln!("  Way index finalized, processing relations...");

    let relation_blocks_buffered = relation_blocks.len();
    // Process buffered relation blocks.
    for block in &relation_blocks {
        block.for_each_element(|element| {
            if let Element::Relation(rel) = element {
                rel_count += 1;
                if let Some(prepared) = prepare_relation(
                    &rel,
                    way_index.as_ref().expect("way_index not returned from drain"),
                    &missing_ref_stats,
                ) {
                    rel_batch_bytes += estimate_prepared_rel_bytes(&prepared);
                    rel_batch.push(prepared);
                    if rel_batch_bytes > max_rel_batch_bytes {
                        max_rel_batch_bytes = rel_batch_bytes;
                    }
                    if rel_batch.len() >= REL_BATCH_SIZE || rel_batch_bytes >= rel_budget {
                        let batch = std::mem::replace(&mut rel_batch, Vec::with_capacity(REL_BATCH_SIZE));
                        rel_batch_bytes = 0;
                        features_emitted += flush_rel_batch(
                            batch, min_z, max_z, &land_mask,
                            sort_writer.as_mut().expect("sort_writer not returned from drain"),
                        );
                    }
                }
            }
        });
    }
    let rss_before_relation_drop = current_rss_kb();
    drop(relation_blocks);
    let rss_after_relation_drop = current_rss_kb();
    let relation_blocks_drop_rss_kb = rss_before_relation_drop.zip(rss_after_relation_drop)
        .map(|(before, after)| before.saturating_sub(after));

    if !rel_batch.is_empty() {
        features_emitted += flush_rel_batch(
            rel_batch, min_z, max_z, &land_mask,
            sort_writer.as_mut().expect("sort_writer not returned from drain"),
        );
    }

    // Drop way_index to release compressed data + index memory before
    // ocean/sort/assemble phases.
    drop(way_index);

    eprintln!("  Nodes: {node_count}, Ways: {way_count}, Relations: {rel_count}");
    eprintln!("  Total features emitted: {features_emitted}");

    // Compute data extent in Mercator [0,1] with generous buffer for ocean overlap
    let data_bounds = if min_lat_e7 < max_lat_e7 {
        let crosses_dateline = crosses_antimeridian(
            min_lon_e7,
            max_lon_e7,
            min_lon_shifted_e7,
            max_lon_shifted_e7,
        );
        let west_lon_e7 = if crosses_dateline { -1_800_000_000 } else { min_lon_e7 };
        let east_lon_e7 = if crosses_dateline { 1_800_000_000 } else { max_lon_e7 };
        let sw = geometry::project_e7(min_lat_e7, west_lon_e7);
        let ne = geometry::project_e7(max_lat_e7, east_lon_e7);
        // Add ~1 degree buffer (in Mercator space, roughly 1/360 ≈ 0.003)
        let buf = 0.01;
        MercBbox {
            min_x: if crosses_dateline { 0.0 } else { (sw.x - buf).max(0.0) },
            min_y: (ne.y - buf).max(0.0),  // ne.y < sw.y in Mercator [0,1]
            max_x: if crosses_dateline { 1.0 } else { (ne.x + buf).min(1.0) },
            max_y: (sw.y + buf).min(1.0),
        }
    } else {
        // No nodes — full world
        MercBbox { min_x: 0.0, min_y: 0.0, max_x: 1.0, max_y: 1.0 }
    };
    eprintln!("  Data bounds (merc): x[{:.4}–{:.4}] y[{:.4}–{:.4}]",
        data_bounds.min_x, data_bounds.max_x, data_bounds.min_y, data_bounds.max_y);
    let land_mask = std::sync::Arc::try_unwrap(land_mask)
        .unwrap_or_else(|_| panic!("land_mask Arc should have single owner after worker join"));
    eprintln!("  Land mask: {} z14 cells populated", land_mask.count_set());

    let max_way_inflight_bytes = way_hwm.load(Ordering::Relaxed);
    let missing_ref_snapshot = missing_ref_stats.snapshot();
    Ok((
        sort_writer.expect("sort_writer not returned from drain"),
        data_bounds,
        land_mask,
        node_store_stats,
        max_way_inflight_bytes,
        max_rel_batch_bytes,
        relation_blocks_buffered,
        relation_blocks_drop_rss_kb,
        missing_ref_snapshot,
    ))
}

// ---------------------------------------------------------------------------
// Node processing (point layers)
// ---------------------------------------------------------------------------

/// Push a single encoded feature into the sort record buffer.
/// Shared by all geometry emitters (point, line, polygon, multipolygon).
#[inline]
fn push_sort_record(
    tile_id: u64,
    osm_id: u64,
    layer: Layer,
    geom_type: GeomType,
    geom_buf: &[u32],
    attrs_buf: &[u8],
    records: &mut Vec<SortRecord>,
) {
    let data = encode_feature_data_with_attrs(osm_id, geom_type, geom_buf, attrs_buf);
    let key = sort::make_sort_key(tile_id, layer as u8, 0);
    records.push(SortRecord { key, data });
}

#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
fn process_node(
    osm_id: u64,
    lat_e7: i32,
    lon_e7: i32,
    tags: &[(&str, &str)],
    min_zoom: u8,
    max_zoom: u8,
    land_mask: &geometry::LandMask,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let tag_helper = Tags(tags);
    let matches = shortbread::match_element(&tag_helper, OsmGeomType::Node);
    if matches.is_empty() {
        return 0;
    }

    let p = geometry::project_e7(lat_e7, lon_e7);
    let pbbox = MercBbox { min_x: p.x, min_y: p.y, max_x: p.x, max_y: p.y };
    land_mask.mark_bbox(&pbbox);
    let mut count: u64 = 0;
    let mut geom_buf: Vec<u32> = Vec::new();
    let mut attrs_buf: Vec<u8> = Vec::new();

    for m in &matches {
        let z_lo = m.min_zoom.max(min_zoom);
        let z_hi = m.max_zoom.min(max_zoom);
        if z_lo > z_hi {
            continue;
        }

        for z in z_lo..=z_hi {
            encode_attrs_bytes(&mut attrs_buf, &m.attrs, z);
            geometry::for_each_tile_in_bbox(&pbbox, z, |tx, ty| {
                let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                let (px, py) = geometry::merc_to_tile_px(&p, tx, ty, z);
                mvt::encode_point(&mut geom_buf, px, py);
                push_sort_record(tile_id, osm_id, m.layer, GeomType::Point, &geom_buf, &attrs_buf, records);
                count += 1;
            });
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Way processing (line + polygon layers) — parallel batch processing
//
// Raw way data (node ref IDs + owned tags) is collected on the main thread,
// then dispatched to rayon where workers do the expensive work in parallel:
// node coord resolution (mmap reads — page faults spread across threads),
// tag matching, projection, simplification, clipping, MVT encoding.
// The serial post-rayon phase does only fast sequential I/O:
// way_index.put() + sort_writer.push().
// ---------------------------------------------------------------------------

/// Raw way data copied from PBF on the main thread. Tags are owned because
/// PBF element borrows don't survive the callback (same pattern as PreparedRelation).
// Tags use String not compact-string: short-lived, mimalloc handles small allocs efficiently.
struct RawWay {
    way_id: i64,
    node_refs: Vec<i64>,
    preserve_node_refs: Vec<i64>,
    coords_e7: Vec<(i32, i32)>,
    tags: Vec<(String, String)>,
}
const _: () = assert!(std::mem::size_of::<RawWay>() == 104);

/// Estimate heap bytes for a block of raw ways (struct + node_refs + tag strings).
fn estimate_raw_ways_bytes(ways: &[RawWay]) -> usize {
    ways.iter().map(|w| {
        std::mem::size_of::<RawWay>() + w.node_refs.len() * 8
            + w.preserve_node_refs.len() * 8
            + w.coords_e7.len() * 8
            + w.tags.len() * 48
            + w.tags.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
    }).sum()
}

/// Mark interior node refs that are shared by at least 2 ways in the same block.
///
/// This preserves common junction vertices during DP simplification without global
/// topology indexing. Block-local detection catches most local road intersections
/// because OSM PBF primitive blocks are spatially clustered.
/// Limitation: cross-block shared nodes are intentionally not detected here.
fn annotate_block_shared_node_refs(raw_ways: &mut [RawWay]) {
    let mut counts: FxHashMap<i64, u8> = FxHashMap::default();
    for w in raw_ways.iter() {
        if w.node_refs.len() <= 2 {
            continue;
        }
        let is_closed = w.node_refs.len() >= 4 && w.node_refs.first() == w.node_refs.last();
        if is_closed {
            // Closed ring: shared-edge vertices can appear anywhere in the ring
            // except the duplicated closing vertex.
            for &node_id in &w.node_refs[..w.node_refs.len() - 1] {
                counts
                    .entry(node_id)
                    .and_modify(|c| *c = c.saturating_add(1))
                    .or_insert(1);
            }
        } else {
            // Open line: preserve interior junctions, but not endpoints.
            for &node_id in &w.node_refs[1..w.node_refs.len() - 1] {
                counts
                    .entry(node_id)
                    .and_modify(|c| *c = c.saturating_add(1))
                    .or_insert(1);
            }
        }
    }

    for w in raw_ways.iter_mut() {
        w.preserve_node_refs.clear();
        if w.node_refs.len() <= 2 {
            continue;
        }
        let is_closed = w.node_refs.len() >= 4 && w.node_refs.first() == w.node_refs.last();
        let scan_slice = if is_closed {
            &w.node_refs[..w.node_refs.len() - 1]
        } else {
            &w.node_refs[1..w.node_refs.len() - 1]
        };
        for &node_id in scan_slice {
            if counts.get(&node_id).is_some_and(|&c| c >= 2)
                && !w.preserve_node_refs.contains(&node_id)
            {
                w.preserve_node_refs.push(node_id);
            }
        }
    }
}

#[inline]
fn merc_point_key(p: &Point) -> (i64, i64) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let x = (p.x * 1_000_000_000_000.0).round() as i64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let y = (p.y * 1_000_000_000_000.0).round() as i64;
    (x, y)
}

fn relation_shared_vertex_keys(member_ways: &[MemberWay]) -> FxHashSet<(i64, i64)> {
    let mut counts: FxHashMap<(i64, i64), u8> = FxHashMap::default();
    let mut local: FxHashSet<(i64, i64)> = FxHashSet::default();
    for mw in member_ways {
        local.clear();
        if mw.coords.len() < 2 {
            continue;
        }
        let is_closed = mw.coords.len() >= 4
            && merc_point_key(&mw.coords[0]) == merc_point_key(&mw.coords[mw.coords.len() - 1]);
        let end = if is_closed {
            mw.coords.len().saturating_sub(1)
        } else {
            mw.coords.len()
        };
        for p in &mw.coords[..end] {
            local.insert(merc_point_key(p));
        }
        for key in &local {
            counts
                .entry(*key)
                .and_modify(|c| *c = c.saturating_add(1))
                .or_insert(1);
        }
    }
    counts
        .into_iter()
        .filter_map(|(k, c)| (c >= 2).then_some(k))
        .collect()
}

#[inline]
fn wrap_unit_x(x: f64) -> f64 {
    x.rem_euclid(1.0)
}

#[inline]
fn lon_e7_shifted_360(lon_e7: i32) -> i64 {
    let lon = i64::from(lon_e7);
    if lon < 0 { lon + LON_E7_FULL_CIRCLE } else { lon }
}

#[inline]
fn crosses_antimeridian(
    min_lon_e7: i32,
    max_lon_e7: i32,
    min_lon_shifted_e7: i64,
    max_lon_shifted_e7: i64,
) -> bool {
    let raw_span_e7 = i64::from(max_lon_e7) - i64::from(min_lon_e7);
    let shifted_span_e7 = max_lon_shifted_e7 - min_lon_shifted_e7;
    shifted_span_e7 < raw_span_e7
}

fn unwrap_antimeridian_path(points: &mut [Point], closed: bool) -> bool {
    if points.len() < 2 {
        return false;
    }
    let mut changed = false;
    for i in 1..points.len() {
        let prev = points[i - 1].x;
        let mut x = points[i].x;
        while x - prev > 0.5 {
            x -= 1.0;
            changed = true;
        }
        while prev - x > 0.5 {
            x += 1.0;
            changed = true;
        }
        points[i].x = x;
    }
    if closed {
        let last = points.len() - 1;
        points[last].x = points[0].x;
        points[last].y = points[0].y;
    }
    changed
}

fn antimeridian_shifts_for_bbox(bbox: &MercBbox) -> SmallVec<[f64; 3]> {
    let mut shifts = SmallVec::new();
    shifts.push(0.0);
    if bbox.min_x < 0.0 {
        shifts.push(1.0);
    }
    if bbox.max_x > 1.0 {
        shifts.push(-1.0);
    }
    shifts
}

fn mark_bbox_wrapped(mask: &geometry::LandMask, bbox: &MercBbox) {
    for shift in antimeridian_shifts_for_bbox(bbox) {
        let shifted = MercBbox {
            min_x: bbox.min_x + shift,
            max_x: bbox.max_x + shift,
            min_y: bbox.min_y,
            max_y: bbox.max_y,
        };
        mask.mark_bbox(&shifted);
    }
}

/// Result of parallel way processing: resolved coords (needed for way_index),
/// sort records (geometry output). Land mask is marked on rayon threads directly.
struct ProcessedWay {
    way_id: i64,
    coords_e7: Vec<(i32, i32)>,
    records: Vec<SortRecord>,
}

struct PointEmitScratch {
    geom_buf: Vec<u32>,
    attrs_buf: Vec<u8>,
}

impl PointEmitScratch {
    fn new() -> Self {
        Self {
            geom_buf: Vec::new(),
            attrs_buf: Vec::new(),
        }
    }
}

struct LineEmitScratch {
    geom_buf: Vec<u32>,
    attrs_buf: Vec<u8>,
    tc_buf: Vec<(i32, i32)>,
    simplify_keep: Vec<bool>,
    simplify_buf: Vec<Point>,
    pinned_idxs: Vec<usize>,
}

impl LineEmitScratch {
    fn new() -> Self {
        Self {
            geom_buf: Vec::new(),
            attrs_buf: Vec::new(),
            tc_buf: Vec::new(),
            simplify_keep: Vec::new(),
            simplify_buf: Vec::new(),
            pinned_idxs: Vec::new(),
        }
    }
}

struct PolygonEmitScratch {
    geom_buf: Vec<u32>,
    attrs_buf: Vec<u8>,
    tc_buf: Vec<(i32, i32)>,
    simplify_keep: Vec<bool>,
    simplify_buf: Vec<Point>,
    pinned_idxs: Vec<usize>,
    clip_a: Vec<Point>,
    clip_b: Vec<Point>,
    row_clip_a: Vec<Point>,
    row_clip_b: Vec<Point>,
}

impl PolygonEmitScratch {
    fn new() -> Self {
        Self {
            geom_buf: Vec::new(),
            attrs_buf: Vec::new(),
            tc_buf: Vec::new(),
            simplify_keep: Vec::new(),
            simplify_buf: Vec::new(),
            pinned_idxs: Vec::new(),
            clip_a: Vec::new(),
            clip_b: Vec::new(),
            row_clip_a: Vec::new(),
            row_clip_b: Vec::new(),
        }
    }
}

struct MultipolygonEmitScratch {
    geom_buf: Vec<u32>,
    attrs_buf: Vec<u8>,
    required_idxs: Vec<usize>,
    clip_a: Vec<Point>,
    clip_b: Vec<Point>,
    all_rings: Vec<Vec<(i32, i32)>>,
    inner_bboxes: Vec<geometry::MercBbox>,
    row_clip_a: Vec<Point>,
    row_clip_b: Vec<Point>,
    row_outer: Vec<Point>,
    row_inners: Vec<Vec<Point>>,
    row_inner_bboxes: Vec<geometry::MercBbox>,
}

impl MultipolygonEmitScratch {
    fn new() -> Self {
        Self {
            geom_buf: Vec::new(),
            attrs_buf: Vec::new(),
            required_idxs: Vec::new(),
            clip_a: Vec::new(),
            clip_b: Vec::new(),
            all_rings: Vec::new(),
            inner_bboxes: Vec::new(),
            row_clip_a: Vec::new(),
            row_clip_b: Vec::new(),
            row_outer: Vec::new(),
            row_inners: Vec::new(),
            row_inner_bboxes: Vec::new(),
        }
    }
}

struct WayWorkerScratch {
    merc: Vec<Point>,
    point_emit: PointEmitScratch,
    line_emit: LineEmitScratch,
    polygon_emit: PolygonEmitScratch,
}

impl WayWorkerScratch {
    fn new() -> Self {
        Self {
            merc: Vec::new(),
            point_emit: PointEmitScratch::new(),
            line_emit: LineEmitScratch::new(),
            polygon_emit: PolygonEmitScratch::new(),
        }
    }
}

thread_local! {
    static WAY_WORKER_SCRATCH: std::cell::RefCell<WayWorkerScratch> = std::cell::RefCell::new(WayWorkerScratch::new());
}

/// Drain a single batch of processed way results: write way_index entries,
/// push sort records. Returns feature count. Land mask marking moved to rayon threads.
#[hotpath::measure]
fn drain_processed_ways(
    results: Vec<ProcessedWay>,
    way_index: &mut WayIndex,
    sort_writer: &mut SortWriter,
) -> u64 {
    let mut count: u64 = 0;
    for pw in results {
        if !pw.coords_e7.is_empty() {
            way_index.put(pw.way_id, &pw.coords_e7);
        }
        count += pw.records.len() as u64;
        // Panic: disk I/O failure is unrecoverable mid-pipeline.
        for record in pw.records {
            sort_writer.push(record).expect("sort push failed");
        }
    }
    count
}

/// Process a raw way on a rayon worker thread: resolve node coordinates,
/// match tags, and run geometry processing (projection, simplification,
/// clipping, MVT encoding).
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
fn process_raw_way(
    raw: &RawWay,
    node_reader: Option<&NodeStoreReader>,
    land_mask: &geometry::LandMask,
    min_zoom: u8,
    max_zoom: u8,
    missing_ref_stats: &MissingRefStatsAtomic,
) -> ProcessedWay {
    // Resolve node coordinates: either pre-resolved from locations-on-ways PBF,
    // or looked up via node store (the expensive mmap reads — now parallel).
    let (coords_e7, resolved_node_refs): (Vec<(i32, i32)>, Vec<i64>) = if !raw.coords_e7.is_empty() {
        (raw.coords_e7.clone(), raw.node_refs.clone())
    } else if let Some(nr) = node_reader {
        let mut missing_refs: usize = 0;
        let mut resolved: Vec<(i32, i32)> = Vec::with_capacity(raw.node_refs.len());
        let mut resolved_refs: Vec<i64> = Vec::with_capacity(raw.node_refs.len());
        for &id in &raw.node_refs {
            if let Some(coord) = nr.get(id) {
                resolved.push(coord);
                resolved_refs.push(id);
            } else {
                missing_refs += 1;
            }
        }
        if missing_refs > 0 {
            missing_ref_stats.record_way_missing_nodes(missing_refs);
        }
        (resolved, resolved_refs)
    } else {
        (Vec::new(), Vec::new())
    };

    if coords_e7.is_empty() || raw.tags.is_empty() {
        return ProcessedWay { way_id: raw.way_id, coords_e7, records: Vec::new() };
    }

    // Tag matching — convert owned tags to borrowed refs (same pattern as
    // process_prepared_relation, pipeline.rs PreparedRelation handling)
    let is_closed = coords_e7.len() >= 4 && coords_e7.first() == coords_e7.last();
    let geom_type = if is_closed { OsmGeomType::ClosedWay } else { OsmGeomType::OpenWay };
    let tags_ref: Vec<(&str, &str)> = raw.tags.iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let tag_helper = Tags(&tags_ref);
    let mut matches = shortbread::match_element(&tag_helper, geom_type);

    if matches.is_empty() {
        return ProcessedWay { way_id: raw.way_id, coords_e7, records: Vec::new() };
    }

    #[allow(clippy::cast_sign_loss)]
    let osm_id = raw.way_id as u64;
    let mut records = Vec::new();
    let mut bbox: Option<MercBbox> = None;
    let mut preserve_vertex_mask: Vec<bool> = vec![false; coords_e7.len()];
    if !raw.preserve_node_refs.is_empty() {
        let preserve_nodes: FxHashSet<i64> = raw.preserve_node_refs.iter().copied().collect();
        for (i, node_id) in resolved_node_refs.iter().enumerate() {
            if preserve_nodes.contains(node_id) {
                preserve_vertex_mask[i] = true;
            }
        }
    }

    WAY_WORKER_SCRATCH.with(|cell| {
        let scratch = &mut *cell.borrow_mut();
        scratch.merc.clear();
        scratch.merc.extend(coords_e7.iter().map(|&(lat, lon)| geometry::project_e7(lat, lon)));
        let _ = unwrap_antimeridian_path(&mut scratch.merc, is_closed);

        let merc = scratch.merc.as_slice();
        let merc_bbox_val = merc_bbox(merc);
        bbox = Some(merc_bbox_val);

        // Enrich polygon matches with area-dependent data (way_area, min_zoom overrides)
        if is_closed {
            let area_m2 = geometry::area_sq_meters(merc);
            enrich_polygon_matches(&mut matches, area_m2);
        }

        for m in &matches {
            let z_lo = m.min_zoom.max(min_zoom);
            let z_hi = m.max_zoom.min(max_zoom);
            if z_lo > z_hi {
                continue;
            }

            match m.geom_expect {
                GeomExpect::Point | GeomExpect::PolygonCentroid | GeomExpect::PolygonPointOnSurface => {
                    emit_point_or_centroid(
                        osm_id, merc, None, &merc_bbox_val, m, z_lo, z_hi, &mut records, &mut scratch.point_emit,
                    );
                }
                GeomExpect::Line => {
                    for shift in antimeridian_shifts_for_bbox(&merc_bbox_val) {
                        if shift == 0.0 {
                            emit_line_feature(
                                osm_id,
                                merc,
                                &preserve_vertex_mask,
                                m,
                                z_lo,
                                z_hi,
                                &mut records,
                                &mut scratch.line_emit,
                            );
                        } else {
                            let shifted: Vec<Point> = merc
                                .iter()
                                .map(|p| Point { x: p.x + shift, y: p.y })
                                .collect();
                            emit_line_feature(
                                osm_id,
                                &shifted,
                                &preserve_vertex_mask,
                                m,
                                z_lo,
                                z_hi,
                                &mut records,
                                &mut scratch.line_emit,
                            );
                        }
                    }
                }
                GeomExpect::Polygon => {
                    for shift in antimeridian_shifts_for_bbox(&merc_bbox_val) {
                        if shift == 0.0 {
                            emit_polygon_feature(
                                osm_id,
                                merc,
                                &preserve_vertex_mask,
                                m,
                                z_lo,
                                z_hi,
                                &mut records,
                                &mut scratch.polygon_emit,
                            );
                        } else {
                            let shifted: Vec<Point> = merc
                                .iter()
                                .map(|p| Point { x: p.x + shift, y: p.y })
                                .collect();
                            emit_polygon_feature(
                                osm_id,
                                &shifted,
                                &preserve_vertex_mask,
                                m,
                                z_lo,
                                z_hi,
                                &mut records,
                                &mut scratch.polygon_emit,
                            );
                        }
                    }
                }
            }
        }
    });

    mark_bbox_wrapped(land_mask, &bbox.expect("bbox set from merc coords"));

    ProcessedWay { way_id: raw.way_id, coords_e7, records }
}

// ---------------------------------------------------------------------------
// Relation processing (multipolygon + boundary lines) — parallel batch processing
// ---------------------------------------------------------------------------

/// A relation with geometry resolved from way_index, ready for parallel processing.
/// Matches are resolved eagerly in `prepare_relation` while PBF borrows are alive,
/// avoiding cloning all relation tags to owned Strings.
struct PreparedRelation {
    osm_id: u64,
    matches: SmallVec<[LayerMatch; 4]>,
    member_ways: Vec<MemberWay>,
    is_boundary: bool,
}

const REL_BATCH_SIZE: usize = 1024;
const REL_BATCH_BUDGET_DEFAULT: usize = 64 * 1024 * 1024; // 64 MB

/// Estimate heap bytes for a single prepared relation (struct + member way coords).
fn estimate_prepared_rel_bytes(r: &PreparedRelation) -> usize {
    std::mem::size_of::<PreparedRelation>()
        + r.member_ways.iter().map(|mw| 32 + mw.coords.len() * 16).sum::<usize>()
}

/// Resolve relation geometry from way_index (serial I/O). Returns None if
/// the relation is not a multipolygon/boundary or has no resolvable member ways.
/// Tag matching runs here while PBF borrows are alive, eliminating the need to
/// clone all relation tags to owned Strings.
#[hotpath::measure]
fn prepare_relation(
    rel: &pbfhogg::Relation<'_>,
    way_index: &WayIndex,
    missing_ref_stats: &MissingRefStatsAtomic,
) -> Option<PreparedRelation> {
    // Fast reject without tag allocation: most relations are not multipolygon/boundary.
    let mut rel_type = "";
    for (k, v) in rel.tags() {
        if k == "type" {
            rel_type = v;
            break;
        }
    }
    if rel_type != "multipolygon" && rel_type != "boundary" {
        return None;
    }
    let tags: SmallVec<[(&str, &str); 16]> = rel.tags().collect();
    let tag_helper = Tags(&tags);

    // Match while PBF borrows are alive — attrs copy only the relevant tag values
    // into Cow::Owned, avoiding cloning ALL tags to String.
    let matches = shortbread::match_element(&tag_helper, OsmGeomType::MultiPolygon);
    if matches.is_empty() {
        return None;
    }

    let is_boundary = tag_helper.has_value("boundary", "administrative");

    let mut member_ways: Vec<MemberWay> = Vec::new();
    let mut had_missing_way_ref = false;

    for member in rel.members() {
        match member.id {
            MemberId::Way(way_id) => {
                let role = WayRole::from_str(member.role().unwrap_or(""));
                if let Some(coords_e7) = way_index.get(way_id) {
                    let merc: Vec<Point> = coords_e7
                        .iter()
                        .map(|&(lat, lon)| geometry::project_e7(lat, lon))
                        .collect();
                    member_ways.push(MemberWay { role, coords: merc });
                } else {
                    had_missing_way_ref = true;
                    missing_ref_stats.record_relation_missing_way_ref();
                }
            }
            MemberId::Relation(_) => {
                missing_ref_stats.record_relation_non_way_member();
                missing_ref_stats.record_relation_nested_member();
            }
            MemberId::Node(_) => {
                missing_ref_stats.record_relation_non_way_member();
            }
            _ => {
                missing_ref_stats.record_relation_non_way_member();
            }
        }
    }
    if had_missing_way_ref {
        missing_ref_stats.record_relation_with_missing_way_refs();
    }

    if member_ways.is_empty() {
        return None;
    }

    #[allow(clippy::cast_sign_loss)]
    Some(PreparedRelation {
        osm_id: rel.id() as u64,
        matches,
        member_ways,
        is_boundary,
    })
}

/// Per-worker accumulator for streaming relation outputs to chunk files.
/// Modeled on `OceanAcc` in ocean.rs — each rayon worker flushes directly
/// to disk, eliminating the `Vec<Vec<SortRecord>>` double-materialization.
struct RelAcc {
    records: Vec<SortRecord>,
    bytes: usize,
    chunk_paths: Vec<std::path::PathBuf>,
    count: u64,
    point_emit: PointEmitScratch,
    line_emit: LineEmitScratch,
    multipolygon_emit: MultipolygonEmitScratch,
    simp_scratch: geometry::SimplifyMultiScratch,
}

impl RelAcc {
    fn flush(&mut self, chunk_dir: &std::path::Path, chunk_id: &std::sync::atomic::AtomicUsize) {
        if self.records.is_empty() {
            return;
        }
        let id = chunk_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = chunk_dir.join(format!("chunk_{id:04}.bin"));
        sort::write_sorted_chunk(&mut self.records, &path)
            .expect("relation chunk write failed");
        self.chunk_paths.push(path);
        self.count += self.records.len() as u64;
        self.records.clear();
        self.bytes = 0;
    }
}

/// Process a batch of prepared relations in parallel, streaming outputs to chunk files.
#[hotpath::measure]
fn flush_rel_batch(
    batch: Vec<PreparedRelation>,
    min_zoom: u8,
    max_zoom: u8,
    land_mask: &geometry::LandMask,
    sort_writer: &mut SortWriter,
) -> u64 {
    use rayon::prelude::*;

    let chunk_id = std::sync::atomic::AtomicUsize::new(sort_writer.chunk_count());
    let chunk_dir = sort_writer.tmp_dir().to_path_buf();
    let chunk_size = sort_writer.chunk_size_bytes();

    let result = batch
        .into_par_iter()
        .fold(
            || RelAcc {
                records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0,
                point_emit: PointEmitScratch::new(),
                line_emit: LineEmitScratch::new(),
                multipolygon_emit: MultipolygonEmitScratch::new(),
                simp_scratch: geometry::SimplifyMultiScratch::new(),
            },
            |mut acc, rel| {
                let before = acc.records.len();
                process_prepared_relation_into(
                    rel, min_zoom, max_zoom, land_mask,
                    &mut acc.records,
                    &mut acc.point_emit,
                    &mut acc.line_emit,
                    &mut acc.multipolygon_emit,
                    &mut acc.simp_scratch,
                );
                for r in &acc.records[before..] {
                    acc.bytes += r.data.len() + std::mem::size_of::<SortRecord>();
                }
                if acc.bytes >= chunk_size {
                    acc.flush(&chunk_dir, &chunk_id);
                }
                acc
            },
        )
        // Don't flush in .map() — collect remaining records back for sort_writer
        // to avoid creating many tiny chunk files (one per rayon accumulator).
        .reduce(
            || RelAcc {
                records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0,
                point_emit: PointEmitScratch::new(),
                line_emit: LineEmitScratch::new(),
                multipolygon_emit: MultipolygonEmitScratch::new(),
                simp_scratch: geometry::SimplifyMultiScratch::new(),
            },
            |mut a, mut b| {
                a.chunk_paths.extend(b.chunk_paths);
                a.count += b.count;
                a.records.append(&mut b.records);
                a.bytes += b.bytes;
                a
            },
        );

    sort_writer.adopt_chunk_files(result.chunk_paths);
    let mut count = result.count;
    // Push remaining records (below chunk_size threshold) through sort_writer's
    // normal buffering, so they merge with way records instead of creating
    // tiny standalone chunk files.
    #[allow(clippy::cast_possible_truncation)]
    {
        count += result.records.len() as u64;
    }
    for record in result.records {
        sort_writer.push(record).expect("sort push failed");
    }
    count
}

/// Process a prepared relation's geometry into an external buffer (CPU-bound).
/// Called from rayon worker threads via `RelAcc` fold. Reuses the caller's
/// `records` vec and `simp_scratch` to avoid per-relation allocation.
#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn process_prepared_relation_into(
    rel: PreparedRelation,
    min_zoom: u8,
    max_zoom: u8,
    land_mask: &geometry::LandMask,
    records: &mut Vec<SortRecord>,
    point_emit: &mut PointEmitScratch,
    line_emit: &mut LineEmitScratch,
    multipolygon_emit: &mut MultipolygonEmitScratch,
    simp_scratch: &mut geometry::SimplifyMultiScratch,
) {
    let multi = multipolygon::assemble(&rel.member_ways);
    let shared_vertex_keys = relation_shared_vertex_keys(&rel.member_ways);

    if multi.polygons.is_empty() {
        return;
    }

    let mut matches = rel.matches;

    let total_area_m2: f64 = multi.polygons.iter()
        .map(|(outer, _)| geometry::area_sq_meters(outer))
        .sum();
    enrich_polygon_matches(&mut matches, total_area_m2);

    for m in &matches {
        let z_lo = m.min_zoom.max(min_zoom);
        let z_hi = m.max_zoom.min(max_zoom);
        if z_lo > z_hi {
            continue;
        }

        match m.geom_expect {
            GeomExpect::Polygon => {
                for (outer, inners) in &multi.polygons {
                    if outer.len() < 4 {
                        continue;
                    }
                    let mut outer_unwrapped = outer.clone();
                    let mut inners_unwrapped = inners.clone();
                    let _ = unwrap_antimeridian_path(&mut outer_unwrapped, true);
                    for inner in &mut inners_unwrapped {
                        let _ = unwrap_antimeridian_path(inner, true);
                    }
                    let bbox = merc_bbox(&outer_unwrapped);
                    mark_bbox_wrapped(land_mask, &bbox);
                    for shift in antimeridian_shifts_for_bbox(&bbox) {
                        if shift == 0.0 {
                            emit_multipolygon_feature(
                                rel.osm_id,
                                &outer_unwrapped,
                                &inners_unwrapped,
                                Some(&shared_vertex_keys),
                                m,
                                z_lo, z_hi, records, multipolygon_emit, simp_scratch,
                            );
                        } else {
                            let outer_shifted: Vec<Point> = outer_unwrapped
                                .iter()
                                .map(|p| Point { x: p.x + shift, y: p.y })
                                .collect();
                            let inners_shifted: Vec<Vec<Point>> = inners_unwrapped
                                .iter()
                                .map(|ring| {
                                    ring.iter()
                                        .map(|p| Point { x: p.x + shift, y: p.y })
                                        .collect()
                                })
                                .collect();
                            emit_multipolygon_feature(
                                rel.osm_id,
                                &outer_shifted,
                                &inners_shifted,
                                Some(&shared_vertex_keys),
                                m,
                                z_lo, z_hi, records, multipolygon_emit, simp_scratch,
                            );
                        }
                    }
                }
            }
            GeomExpect::PolygonCentroid | GeomExpect::PolygonPointOnSurface => {
                for (outer, inners) in &multi.polygons {
                    if outer.len() < 4 {
                        continue;
                    }
                    let mut outer_unwrapped = outer.clone();
                    let mut inners_unwrapped = inners.clone();
                    let _ = unwrap_antimeridian_path(&mut outer_unwrapped, true);
                    for inner in &mut inners_unwrapped {
                        let _ = unwrap_antimeridian_path(inner, true);
                    }
                    let bbox = merc_bbox(&outer_unwrapped);
                    mark_bbox_wrapped(land_mask, &bbox);
                    emit_point_or_centroid(
                        rel.osm_id,
                        &outer_unwrapped,
                        Some(&inners_unwrapped),
                        &bbox,
                        m,
                        z_lo,
                        z_hi,
                        records,
                        point_emit,
                    );
                }
            }
            GeomExpect::Line => {
                if !rel.is_boundary {
                    continue;
                }
                for mw in &rel.member_ways {
                    if mw.coords.len() < 2 {
                        continue;
                    }
                    let mut coords = mw.coords.clone();
                    let _ = unwrap_antimeridian_path(&mut coords, false);
                    let bbox = merc_bbox(&coords);
                    mark_bbox_wrapped(land_mask, &bbox);
                    for shift in antimeridian_shifts_for_bbox(&bbox) {
                        if shift == 0.0 {
                            emit_line_feature(
                                rel.osm_id,
                                &coords,
                                &[],
                                m,
                                z_lo,
                                z_hi,
                                records,
                                line_emit,
                            );
                        } else {
                            let shifted: Vec<Point> = coords
                                .iter()
                                .map(|p| Point { x: p.x + shift, y: p.y })
                                .collect();
                            emit_line_feature(
                                rel.osm_id,
                                &shifted,
                                &[],
                                m,
                                z_lo,
                                z_hi,
                                records,
                                line_emit,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Polygon enrichment (area-dependent attrs + min_zoom overrides)
// ---------------------------------------------------------------------------

/// Enrich polygon matches with geometry-dependent data.
/// - BoundaryLabels: `way_area` in hectares, min_zoom override based on area thresholds.
fn enrich_polygon_matches(matches: &mut [LayerMatch], area_m2: f64) {
    for m in matches.iter_mut() {
        if m.layer == Layer::BoundaryLabels {
            // Add way_area in hectares
            let hectares = area_m2 / 10_000.0;
            m.attrs.push(("way_area", AttrValue::Float(hectares), 0));

            // Override min_zoom based on area (Planetiler thresholds)
            let area_km2 = area_m2 / 1e6;
            let admin_level = m.attrs.iter()
                .find(|(k, _, _)| *k == "admin_level")
                .and_then(|(_, v, _)| if let AttrValue::Int(n) = v { Some(*n) } else { None })
                .unwrap_or(0);

            if admin_level == 2 && area_km2 >= 2_000_000.0 {
                m.min_zoom = 2;
            } else if area_km2 >= 700_000.0 {
                m.min_zoom = 3;
            } else if area_km2 >= 100_000.0 {
                m.min_zoom = 4;
            }
            // else stays at 5 (default from match_boundary_labels)
        }
    }
}

fn orient2d(a: (i32, i32), b: (i32, i32), c: (i32, i32)) -> i64 {
    let abx = i64::from(b.0) - i64::from(a.0);
    let aby = i64::from(b.1) - i64::from(a.1);
    let acx = i64::from(c.0) - i64::from(a.0);
    let acy = i64::from(c.1) - i64::from(a.1);
    abx * acy - aby * acx
}

fn on_segment(a: (i32, i32), b: (i32, i32), p: (i32, i32)) -> bool {
    let (min_x, max_x) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
    let (min_y, max_y) = if a.1 <= b.1 { (a.1, b.1) } else { (b.1, a.1) };
    p.0 >= min_x && p.0 <= max_x && p.1 >= min_y && p.1 <= max_y
}

fn segments_intersect(a1: (i32, i32), a2: (i32, i32), b1: (i32, i32), b2: (i32, i32)) -> bool {
    let o1 = orient2d(a1, a2, b1);
    let o2 = orient2d(a1, a2, b2);
    let o3 = orient2d(b1, b2, a1);
    let o4 = orient2d(b1, b2, a2);

    if o1 == 0 && on_segment(a1, a2, b1) {
        return true;
    }
    if o2 == 0 && on_segment(a1, a2, b2) {
        return true;
    }
    if o3 == 0 && on_segment(b1, b2, a1) {
        return true;
    }
    if o4 == 0 && on_segment(b1, b2, a2) {
        return true;
    }
    (o1 > 0) != (o2 > 0) && (o3 > 0) != (o4 > 0)
}

fn is_valid_simple_tile_ring(ring: &[(i32, i32)]) -> bool {
    if ring.len() < 4 || ring.first() != ring.last() {
        return false;
    }
    let edge_count = ring.len() - 1;
    for i in 0..edge_count {
        let a1 = ring[i];
        let a2 = ring[i + 1];
        for j in (i + 1)..edge_count {
            if j == i || j == i + 1 || (i == 0 && j == edge_count - 1) {
                continue;
            }
            let b1 = ring[j];
            let b2 = ring[j + 1];
            if segments_intersect(a1, a2, b1, b2) {
                return false;
            }
        }
    }
    true
}

fn orient2d_f64(a: &Point, b: &Point, c: &Point) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn on_segment_f64(a: &Point, b: &Point, p: &Point) -> bool {
    let min_x = a.x.min(b.x);
    let max_x = a.x.max(b.x);
    let min_y = a.y.min(b.y);
    let max_y = a.y.max(b.y);
    p.x >= min_x && p.x <= max_x && p.y >= min_y && p.y <= max_y
}

fn segments_intersect_f64(a1: &Point, a2: &Point, b1: &Point, b2: &Point) -> bool {
    let o1 = orient2d_f64(a1, a2, b1);
    let o2 = orient2d_f64(a1, a2, b2);
    let o3 = orient2d_f64(b1, b2, a1);
    let o4 = orient2d_f64(b1, b2, a2);
    let eps = 1e-15;

    if o1.abs() <= eps && on_segment_f64(a1, a2, b1) {
        return true;
    }
    if o2.abs() <= eps && on_segment_f64(a1, a2, b2) {
        return true;
    }
    if o3.abs() <= eps && on_segment_f64(b1, b2, a1) {
        return true;
    }
    if o4.abs() <= eps && on_segment_f64(b1, b2, a2) {
        return true;
    }
    (o1 > eps) != (o2 > eps) && (o3 > eps) != (o4 > eps)
}

fn is_valid_simple_ring_points(ring: &[Point]) -> bool {
    if ring.len() < 3 {
        return false;
    }
    // Accept both open rings [A,B,C] and closed rings [A,B,C,A].
    let is_closed = if ring.len() >= 4 {
        let first = &ring[0];
        let last = &ring[ring.len() - 1];
        (first.x - last.x).abs() < 1e-15 && (first.y - last.y).abs() < 1e-15
    } else {
        false
    };
    let n = if is_closed { ring.len() - 1 } else { ring.len() };
    if n < 3 {
        return false;
    }

    for i in 0..n {
        let a1 = &ring[i];
        let a2 = &ring[(i + 1) % n];
        for j in (i + 1)..n {
            if j == i || j == (i + 1) % n || (i == 0 && j == n - 1) {
                continue;
            }
            let b1 = &ring[j];
            let b2 = &ring[(j + 1) % n];
            if segments_intersect_f64(a1, a2, b1, b2) {
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Feature emission helpers
// ---------------------------------------------------------------------------

#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
fn emit_point_or_centroid(
    osm_id: u64,
    coords: &[Point],
    inners: Option<&[Vec<Point>]>,
    _bbox: &MercBbox,
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
    scratch: &mut PointEmitScratch,
) -> u64 {
    if coords.is_empty() {
        return 0;
    }
    let pt = if m.geom_expect == GeomExpect::PolygonPointOnSurface {
        if let Some(holes) = inners {
            geometry::point_on_surface_with_holes(coords, holes)
        } else {
            geometry::point_on_surface(coords)
        }
    } else {
        Some(centroid_of(coords))
    };
    let Some(mut p) = pt else { return 0 };
    p.x = wrap_unit_x(p.x);

    let cbbox = MercBbox { min_x: p.x, min_y: p.y, max_x: p.x, max_y: p.y };
    let mut count: u64 = 0;
    for z in z_lo..=z_hi {
        encode_attrs_bytes(&mut scratch.attrs_buf, &m.attrs, z);
        geometry::for_each_tile_in_bbox(&cbbox, z, |tx, ty| {
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            let (px, py) = geometry::merc_to_tile_px(&p, tx, ty, z);
            mvt::encode_point(&mut scratch.geom_buf, px, py);
            push_sort_record(tile_id, osm_id, m.layer, GeomType::Point, &scratch.geom_buf, &scratch.attrs_buf, records);
            count += 1;
        });
    }
    count
}

#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
fn emit_line_feature(
    osm_id: u64,
    merc: &[Point],
    preserve_vertex_mask: &[bool],
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
    scratch: &mut LineEmitScratch,
) -> u64 {
    let mut count: u64 = 0;
    scratch.pinned_idxs.clear();
    scratch.pinned_idxs.extend(
        preserve_vertex_mask
            .iter()
            .enumerate()
            .filter_map(|(i, &keep)| keep.then_some(i)),
    );
    let has_pins = !scratch.pinned_idxs.is_empty();
    let mut run_for_zoom = |z: u8, simplified: &[Point]| {
        encode_attrs_bytes(&mut scratch.attrs_buf, &m.attrs, z);

        // Recompute bbox from simplified coords — at low zooms DP may reduce the
        // geometry to far fewer tiles than the original bbox suggests.
        let simp_bbox = merc_bbox(simplified);
        let single_tile = geometry::is_single_tile(&simp_bbox, z);

        // Skip min-size filtering at max zoom and for boundaries/streets
        let skip_size_filter = z >= 14
            || m.layer == Layer::Boundaries
            || m.layer == Layer::Streets;
        geometry::for_each_tile_in_bbox(&simp_bbox, z, |tx, ty| {
            if single_tile {
                // Fast path: bbox fits in one tile — clipping is a no-op.
                if simplified.len() < 2 {
                    return;
                }
                geometry::to_tile_coords_into(&mut scratch.tc_buf, simplified, tx, ty, z);
                if !skip_size_filter && geometry::line_is_subpixel(&scratch.tc_buf) {
                    return;
                }
                mvt::encode_linestring(&mut scratch.geom_buf, &scratch.tc_buf);
                if scratch.geom_buf.is_empty() {
                    return;
                }
                let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                push_sort_record(tile_id, osm_id, m.layer, GeomType::LineString, &scratch.geom_buf, &scratch.attrs_buf, records);
                count += 1;
            } else {
                let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);
                geometry::for_each_clipped_segment(simplified, &clip, |segment| {
                    if segment.len() < 2 {
                        return;
                    }
                    geometry::to_tile_coords_into(&mut scratch.tc_buf, segment, tx, ty, z);
                    if !skip_size_filter && geometry::line_is_subpixel(&scratch.tc_buf) {
                        return;
                    }
                    mvt::encode_linestring(&mut scratch.geom_buf, &scratch.tc_buf);
                    if scratch.geom_buf.is_empty() {
                        return;
                    }
                    let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                    push_sort_record(tile_id, osm_id, m.layer, GeomType::LineString, &scratch.geom_buf, &scratch.attrs_buf, records);
                    count += 1;
                });
            }
        });
    };

    if has_pins {
        for z in (z_lo..=z_hi).rev() {
            if z < 14 && geometry::merc_bbox_is_subpixel(merc, z) {
                break;
            }
            if z < 14 {
                let tol = geometry::simplify_tolerance(z);
                let _ = geometry::simplify_into_with_required(
                    merc,
                    tol,
                    &scratch.pinned_idxs,
                    &mut scratch.simplify_keep,
                    &mut scratch.simplify_buf,
                );
                if scratch.simplify_buf.len() < 2 {
                    break;
                }
                run_for_zoom(z, &scratch.simplify_buf);
            } else {
                run_for_zoom(z, merc);
            }
        }
    } else {
        geometry::for_each_zoom_simplified(merc, z_lo, z_hi, 2, |z, simplified| {
            run_for_zoom(z, simplified);
        });
    }
    count
}

#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn emit_polygon_feature(
    osm_id: u64,
    merc: &[Point],
    preserve_vertex_mask: &[bool],
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
    scratch: &mut PolygonEmitScratch,
) -> u64 {
    // Single-ring polygon (no holes)
    let mut count: u64 = 0;
    scratch.pinned_idxs.clear();
    scratch.pinned_idxs.extend(
        preserve_vertex_mask
            .iter()
            .enumerate()
            .filter_map(|(i, &keep)| keep.then_some(i)),
    );
    let has_pins = !scratch.pinned_idxs.is_empty();
    let mut run_for_zoom = |z: u8, simplified: &[Point]| {
        encode_attrs_bytes(&mut scratch.attrs_buf, &m.attrs, z);

        // Recompute bbox from simplified coords — at low zooms DP may reduce the
        // geometry to far fewer tiles than the original bbox suggests.
        let simp_bbox = merc_bbox(simplified);
        let single_tile = geometry::is_single_tile(&simp_bbox, z);
        let skip_size_filter = z >= 14;
        let (tx_min, tx_max, ty_min, ty_max) = geometry::tile_range_in_bbox(&simp_bbox, z);

        if single_tile {
            // Fast path: bbox fits in one tile — clipping is a no-op.
            let (tx, ty) = (tx_min, ty_min);
            if simplified.len() < 3 {
                return;
            }
            if z < 14 && !is_valid_simple_ring_points(simplified) {
                return;
            }
            geometry::to_tile_coords_into(&mut scratch.tc_buf, simplified, tx, ty, z);
            if !skip_size_filter && geometry::ring_is_subpixel(&scratch.tc_buf) {
                return;
            }
            close_and_orient_cw(&mut scratch.tc_buf);
            if z < 14 && !is_valid_simple_tile_ring(&scratch.tc_buf) {
                return;
            }

            mvt::encode_polygon(&mut scratch.geom_buf, &[&scratch.tc_buf]);
            if scratch.geom_buf.is_empty() {
                return;
            }
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            push_sort_record(tile_id, osm_id, m.layer, GeomType::Polygon, &scratch.geom_buf, &scratch.attrs_buf, records);
            count += 1;
        } else {
            let inv_z = 1.0 / f64::from(1u32 << z);
            let tile_buf = BUFFER_FRACTION * inv_z;
            let multi_row = ty_max > ty_min;

            for ty in ty_min..=ty_max {
                // F14: row pre-clip — restrict polygon to this row's Y-band.
                // Per-tile clips then process far fewer vertices.
                let row_source: &[Point] = if multi_row {
                    let row_rect = ClipRect::new(
                        0.0,
                        f64::from(ty) * inv_z - tile_buf,
                        1.0,
                        f64::from(ty + 1) * inv_z + tile_buf,
                    );
                    geometry::clip_polygon_into(
                        simplified, &row_rect, &mut scratch.row_clip_a, &mut scratch.row_clip_b,
                    );
                    if scratch.row_clip_a.len() < 3 {
                        continue;
                    }
                    &scratch.row_clip_a
                } else {
                    simplified
                };

                for tx in tx_min..=tx_max {
                    let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);
                    if geometry::tile_is_interior(row_source, &clip) {
                        scratch.tc_buf.clear();
                        scratch.tc_buf.extend_from_slice(&INTERIOR_TILE_RING);
                    } else {
                        geometry::clip_polygon_into(
                            row_source, &clip, &mut scratch.clip_a, &mut scratch.clip_b,
                        );
                        if scratch.clip_a.len() < 3 {
                            continue;
                        }
                        if z < 14 && !is_valid_simple_ring_points(&scratch.clip_a) {
                            continue;
                        }
                        geometry::to_tile_coords_into(&mut scratch.tc_buf, &scratch.clip_a, tx, ty, z);
                        if !skip_size_filter && geometry::ring_is_subpixel(&scratch.tc_buf) {
                            continue;
                        }
                        close_and_orient_cw(&mut scratch.tc_buf);
                        if z < 14 && !is_valid_simple_tile_ring(&scratch.tc_buf) {
                            continue;
                        }
                    }

                    mvt::encode_polygon(&mut scratch.geom_buf, &[&scratch.tc_buf]);
                    if scratch.geom_buf.is_empty() {
                        continue;
                    }
                    let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                    push_sort_record(tile_id, osm_id, m.layer, GeomType::Polygon, &scratch.geom_buf, &scratch.attrs_buf, records);
                    count += 1;
                }
            }
        }
    };
    if m.layer == Layer::Boundaries {
        // Boundary mode: single zoom loop.
        // z <= 8: emit full-res geometry (assemble-phase reconciliation + tile-coord DP).
        // z 9..13: normal DP simplification.
        // z >= 14: full-res (existing behavior, no simplification).
        for z in (z_lo..=z_hi).rev() {
            if z < 14 && geometry::merc_bbox_is_subpixel(merc, z) {
                break;
            }
            if z <= BOUNDARY_NO_SIMP_MAX || z >= 14 {
                run_for_zoom(z, merc);
            } else {
                let tol = geometry::simplify_tolerance(z);
                if has_pins {
                    let _ = geometry::simplify_into_with_required(
                        merc,
                        tol,
                        &scratch.pinned_idxs,
                        &mut scratch.simplify_keep,
                        &mut scratch.simplify_buf,
                    );
                } else {
                    let _ = geometry::simplify_into(
                        merc,
                        tol,
                        &mut scratch.simplify_keep,
                        &mut scratch.simplify_buf,
                    );
                }
                if scratch.simplify_buf.len() < 4 {
                    break;
                }
                run_for_zoom(z, &scratch.simplify_buf);
            }
        }
    } else if has_pins {
        for z in (z_lo..=z_hi).rev() {
            if z < 14 && geometry::merc_bbox_is_subpixel(merc, z) {
                break;
            }
            if z < 14 {
                let tol = geometry::simplify_tolerance(z);
                let _ = geometry::simplify_into_with_required(
                    merc,
                    tol,
                    &scratch.pinned_idxs,
                    &mut scratch.simplify_keep,
                    &mut scratch.simplify_buf,
                );
                if scratch.simplify_buf.len() < 4 {
                    break;
                }
                run_for_zoom(z, &scratch.simplify_buf);
            } else {
                run_for_zoom(z, merc);
            }
        }
    } else {
        geometry::for_each_zoom_simplified(merc, z_lo, z_hi, 4, |z, simplified| {
            run_for_zoom(z, simplified);
        });
    }
    count
}

#[hotpath::measure]
#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::cognitive_complexity)]
fn emit_multipolygon_feature(
    osm_id: u64,
    outer: &[Point],
    inners: &[Vec<Point>],
    preserve_vertex_keys: Option<&FxHashSet<(i64, i64)>>,
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
    emit_scratch: &mut MultipolygonEmitScratch,
    simp_scratch: &mut geometry::SimplifyMultiScratch,
) -> u64 {
    let mut count: u64 = 0;
    let mut emit_for_zoom = |z: u8, simp_outer: &[Point], simp_inners: &[Vec<Point>]| {
        encode_attrs_bytes(&mut emit_scratch.attrs_buf, &m.attrs, z);

        // Recompute bbox from simplified coords — at low zooms DP may reduce the
        // geometry to far fewer tiles than the original bbox suggests.
        let simp_bbox = merc_bbox(simp_outer);

        // F11: Precompute inner ring bboxes for O(1) tile rejection.
        emit_scratch.inner_bboxes.clear();
        emit_scratch.inner_bboxes.extend(simp_inners.iter().map(|r| merc_bbox(r)));
        let single_tile = geometry::is_single_tile(&simp_bbox, z);
        let skip_size_filter = z >= 14;
        let (tx_min, tx_max, ty_min, ty_max) = geometry::tile_range_in_bbox(&simp_bbox, z);

        if single_tile {
            // Fast path: bbox fits in one tile — clipping is a no-op.
            let (tx, ty) = (tx_min, ty_min);
            let mut ring_count: usize = 0;
            if simp_outer.len() < 3 {
                return;
            }
            if z < 14 && !is_valid_simple_ring_points(simp_outer) {
                return;
            }
            if ring_count >= emit_scratch.all_rings.len() { emit_scratch.all_rings.push(Vec::new()); }
            geometry::to_tile_coords_into(&mut emit_scratch.all_rings[ring_count], simp_outer, tx, ty, z);
            if !skip_size_filter && geometry::ring_is_subpixel(&emit_scratch.all_rings[ring_count]) {
                return;
            }
            close_and_orient_cw(&mut emit_scratch.all_rings[ring_count]);
            if z < 14 && !is_valid_simple_tile_ring(&emit_scratch.all_rings[ring_count]) {
                return;
            }
            ring_count += 1;
            for inner in simp_inners {
                if inner.len() < 3 {
                    continue;
                }
                if z < 14 && !is_valid_simple_ring_points(inner) {
                    continue;
                }
                if ring_count >= emit_scratch.all_rings.len() { emit_scratch.all_rings.push(Vec::new()); }
                geometry::to_tile_coords_into(&mut emit_scratch.all_rings[ring_count], inner, tx, ty, z);
                if !skip_size_filter && geometry::ring_is_subpixel(&emit_scratch.all_rings[ring_count]) {
                    continue;
                }
                close_and_orient_ccw(&mut emit_scratch.all_rings[ring_count]);
                if z < 14 && !is_valid_simple_tile_ring(&emit_scratch.all_rings[ring_count]) {
                    continue;
                }
                ring_count += 1;
            }

            let ring_refs: SmallVec<[&[(i32, i32)]; 4]> = emit_scratch.all_rings[..ring_count].iter().map(Vec::as_slice).collect();
            mvt::encode_polygon(&mut emit_scratch.geom_buf, &ring_refs);
            if emit_scratch.geom_buf.is_empty() {
                return;
            }
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            push_sort_record(tile_id, osm_id, m.layer, GeomType::Polygon, &emit_scratch.geom_buf, &emit_scratch.attrs_buf, records);
            count += 1;
        } else {
            let inv_z = 1.0 / f64::from(1u32 << z);
            let tile_buf = BUFFER_FRACTION * inv_z;
            let multi_row = ty_max > ty_min;

            for ty in ty_min..=ty_max {
                // F14: row pre-clip — restrict polygon to this row's Y-band.
                let (outer_src, inners_src, inners_bbox_src): (&[Point], &[Vec<Point>], &[geometry::MercBbox]) =
                    if multi_row {
                        let row_rect = ClipRect::new(
                            0.0,
                            f64::from(ty) * inv_z - tile_buf,
                            1.0,
                            f64::from(ty + 1) * inv_z + tile_buf,
                        );
                        // Pre-clip outer to row.
                        geometry::clip_polygon_into(
                            simp_outer, &row_rect, &mut emit_scratch.row_clip_a, &mut emit_scratch.row_clip_b,
                        );
                        if emit_scratch.row_clip_a.len() < 3 {
                            continue;
                        }
                        std::mem::swap(&mut emit_scratch.row_outer, &mut emit_scratch.row_clip_a);

                        // Pre-clip inners to row.
                        let row_y_min = f64::from(ty) * inv_z - tile_buf;
                        let row_y_max = f64::from(ty + 1) * inv_z + tile_buf;
                        let mut row_inner_count = 0;
                        for (inner, ib) in simp_inners.iter().zip(&emit_scratch.inner_bboxes) {
                            if ib.max_y < row_y_min || ib.min_y > row_y_max {
                                continue;
                            }
                            geometry::clip_polygon_into(
                                inner, &row_rect, &mut emit_scratch.row_clip_a, &mut emit_scratch.row_clip_b,
                            );
                            if emit_scratch.row_clip_a.len() < 3 {
                                continue;
                            }
                            if row_inner_count < emit_scratch.row_inners.len() {
                                std::mem::swap(
                                    &mut emit_scratch.row_inners[row_inner_count],
                                    &mut emit_scratch.row_clip_a,
                                );
                            } else {
                                emit_scratch.row_inners.push(Vec::new());
                                std::mem::swap(
                                    emit_scratch.row_inners.last_mut().expect("just pushed"),
                                    &mut emit_scratch.row_clip_a,
                                );
                            }
                            row_inner_count += 1;
                        }
                        emit_scratch.row_inner_bboxes.clear();
                        emit_scratch.row_inner_bboxes.extend(
                            emit_scratch.row_inners[..row_inner_count].iter().map(|r| merc_bbox(r))
                        );
                        (
                            &emit_scratch.row_outer,
                            &emit_scratch.row_inners[..row_inner_count],
                            &emit_scratch.row_inner_bboxes,
                        )
                    } else {
                        (simp_outer, simp_inners, emit_scratch.inner_bboxes.as_slice())
                    };

                for tx in tx_min..=tx_max {
                    let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);
                    let mut ring_count: usize = 0;

                    if geometry::tile_is_interior(outer_src, &clip) {
                        // Interior tile: outer ring covers entire tile.
                        if ring_count >= emit_scratch.all_rings.len() { emit_scratch.all_rings.push(Vec::new()); }
                        emit_scratch.all_rings[ring_count].clear();
                        emit_scratch.all_rings[ring_count].extend_from_slice(&INTERIOR_TILE_RING);
                        ring_count += 1;
                    } else {
                        geometry::clip_polygon_into(
                            outer_src, &clip, &mut emit_scratch.clip_a, &mut emit_scratch.clip_b,
                        );
                        if emit_scratch.clip_a.len() < 3 {
                            continue;
                        }
                        if z < 14 && !is_valid_simple_ring_points(&emit_scratch.clip_a) {
                            continue;
                        }
                        if ring_count >= emit_scratch.all_rings.len() { emit_scratch.all_rings.push(Vec::new()); }
                        geometry::to_tile_coords_into(&mut emit_scratch.all_rings[ring_count], &emit_scratch.clip_a, tx, ty, z);
                        if !skip_size_filter && geometry::ring_is_subpixel(&emit_scratch.all_rings[ring_count]) {
                            continue;
                        }
                        close_and_orient_cw(&mut emit_scratch.all_rings[ring_count]);
                        if z < 14 && !is_valid_simple_tile_ring(&emit_scratch.all_rings[ring_count]) {
                            continue;
                        }
                        ring_count += 1;
                    }
                    // Inner rings: still need per-tile clipping (holes may be visible).
                    for (inner, ib) in inners_src.iter().zip(inners_bbox_src) {
                        if !geometry::bbox_intersects_clip(ib, &clip) { continue; }
                        geometry::clip_polygon_into(
                            inner, &clip, &mut emit_scratch.clip_a, &mut emit_scratch.clip_b,
                        );
                        if emit_scratch.clip_a.len() < 3 {
                            continue;
                        }
                        if z < 14 && !is_valid_simple_ring_points(&emit_scratch.clip_a) {
                            continue;
                        }
                        if ring_count >= emit_scratch.all_rings.len() { emit_scratch.all_rings.push(Vec::new()); }
                        geometry::to_tile_coords_into(&mut emit_scratch.all_rings[ring_count], &emit_scratch.clip_a, tx, ty, z);
                        if !skip_size_filter && geometry::ring_is_subpixel(&emit_scratch.all_rings[ring_count]) {
                            continue;
                        }
                        close_and_orient_ccw(&mut emit_scratch.all_rings[ring_count]);
                        if z < 14 && !is_valid_simple_tile_ring(&emit_scratch.all_rings[ring_count]) {
                            continue;
                        }
                        ring_count += 1;
                    }

                    let ring_refs: SmallVec<[&[(i32, i32)]; 4]> =
                        emit_scratch.all_rings[..ring_count].iter().map(Vec::as_slice).collect();
                    mvt::encode_polygon(&mut emit_scratch.geom_buf, &ring_refs);
                    if emit_scratch.geom_buf.is_empty() {
                        continue;
                    }
                    let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                    push_sort_record(tile_id, osm_id, m.layer, GeomType::Polygon, &emit_scratch.geom_buf, &emit_scratch.attrs_buf, records);
                    count += 1;
                }
            }
        }
    };
    let has_keys = preserve_vertex_keys.is_some_and(|k| !k.is_empty());
    if m.layer == Layer::Boundaries {
        // Boundary mode: single zoom loop.
        // z <= 8: emit full-res geometry (assemble-phase reconciliation + tile-coord DP).
        // z 9..13: normal DP simplification (with or without preserve_vertex_keys).
        // z >= 14: full-res (existing behavior, no simplification).
        simp_scratch.cascade_outer.clear();
        simp_scratch.cascade_outer.extend_from_slice(outer);
        simp_scratch.cascade_inners.clear();
        simp_scratch.cascade_inners.extend(inners.iter().cloned());

        for z in (z_lo..=z_hi).rev() {
            if z < 14 && geometry::merc_bbox_is_subpixel(&simp_scratch.cascade_outer, z) {
                break;
            }
            if z <= BOUNDARY_NO_SIMP_MAX || z >= 14 {
                emit_for_zoom(z, outer, inners);
            } else {
                // z 9..13: DP simplification.
                let tol = geometry::simplify_tolerance(z);
                if simp_scratch.cascade_outer.len() > 4 {
                    if has_keys {
                        let keys = preserve_vertex_keys.expect("checked above");
                        emit_scratch.required_idxs.clear();
                        let outer_end = simp_scratch.cascade_outer.len().saturating_sub(1);
                        for (i, p) in simp_scratch.cascade_outer.iter().take(outer_end).enumerate() {
                            if keys.contains(&merc_point_key(p)) {
                                emit_scratch.required_idxs.push(i);
                            }
                        }
                        let _ = geometry::simplify_into_with_required(
                            &simp_scratch.cascade_outer,
                            tol,
                            &emit_scratch.required_idxs,
                            &mut simp_scratch.keep_buf,
                            &mut simp_scratch.simp_buf,
                        );
                    } else {
                        let _ = geometry::simplify_into(
                            &simp_scratch.cascade_outer,
                            tol,
                            &mut simp_scratch.keep_buf,
                            &mut simp_scratch.simp_buf,
                        );
                    }
                    std::mem::swap(&mut simp_scratch.cascade_outer, &mut simp_scratch.simp_buf);
                }

                for inner in &mut simp_scratch.cascade_inners {
                    if inner.len() <= 4 {
                        continue;
                    }
                    if has_keys {
                        let keys = preserve_vertex_keys.expect("checked above");
                        emit_scratch.required_idxs.clear();
                        let inner_end = inner.len().saturating_sub(1);
                        for (i, p) in inner.iter().take(inner_end).enumerate() {
                            if keys.contains(&merc_point_key(p)) {
                                emit_scratch.required_idxs.push(i);
                            }
                        }
                        let _ = geometry::simplify_into_with_required(
                            inner,
                            tol,
                            &emit_scratch.required_idxs,
                            &mut simp_scratch.keep_buf,
                            &mut simp_scratch.simp_buf,
                        );
                    } else {
                        let _ = geometry::simplify_into(
                            inner,
                            tol,
                            &mut simp_scratch.keep_buf,
                            &mut simp_scratch.simp_buf,
                        );
                    }
                    std::mem::swap(inner, &mut simp_scratch.simp_buf);
                }
                simp_scratch.cascade_inners.retain(|r| r.len() >= 4);

                if simp_scratch.cascade_outer.len() < 4 {
                    break;
                }
                emit_for_zoom(z, &simp_scratch.cascade_outer, &simp_scratch.cascade_inners);
            }
        }
    } else if has_keys {
        let keys = preserve_vertex_keys.expect("checked above");
        simp_scratch.cascade_outer.clear();
        simp_scratch.cascade_outer.extend_from_slice(outer);
        simp_scratch.cascade_inners.clear();
        simp_scratch.cascade_inners.extend(inners.iter().cloned());

        for z in (z_lo..=z_hi).rev() {
            if z < 14 {
                if geometry::merc_bbox_is_subpixel(&simp_scratch.cascade_outer, z) {
                    break;
                }
                let tol = geometry::simplify_tolerance(z);
                if simp_scratch.cascade_outer.len() > 4 {
                    emit_scratch.required_idxs.clear();
                    let outer_end = simp_scratch.cascade_outer.len().saturating_sub(1);
                    for (i, p) in simp_scratch.cascade_outer.iter().take(outer_end).enumerate() {
                        if keys.contains(&merc_point_key(p)) {
                            emit_scratch.required_idxs.push(i);
                        }
                    }
                    let _ = geometry::simplify_into_with_required(
                        &simp_scratch.cascade_outer,
                        tol,
                        &emit_scratch.required_idxs,
                        &mut simp_scratch.keep_buf,
                        &mut simp_scratch.simp_buf,
                    );
                    std::mem::swap(&mut simp_scratch.cascade_outer, &mut simp_scratch.simp_buf);
                }

                for inner in &mut simp_scratch.cascade_inners {
                    if inner.len() <= 4 {
                        continue;
                    }
                    emit_scratch.required_idxs.clear();
                    let inner_end = inner.len().saturating_sub(1);
                    for (i, p) in inner.iter().take(inner_end).enumerate() {
                        if keys.contains(&merc_point_key(p)) {
                            emit_scratch.required_idxs.push(i);
                        }
                    }
                    let _ = geometry::simplify_into_with_required(
                        inner,
                        tol,
                        &emit_scratch.required_idxs,
                        &mut simp_scratch.keep_buf,
                        &mut simp_scratch.simp_buf,
                    );
                    std::mem::swap(inner, &mut simp_scratch.simp_buf);
                }
                simp_scratch.cascade_inners.retain(|r| r.len() >= 4);
            }
            if simp_scratch.cascade_outer.len() < 4 {
                break;
            }
            emit_for_zoom(z, &simp_scratch.cascade_outer, &simp_scratch.cascade_inners);
        }
    } else {
        geometry::for_each_zoom_simplified_multi(outer, inners, z_lo, z_hi, simp_scratch, |z, simp_outer, simp_inners| {
            emit_for_zoom(z, simp_outer, simp_inners);
        });
    }
    count
}

// ---------------------------------------------------------------------------
// Phase 4: Tile assembly + PMTiles write
// ---------------------------------------------------------------------------

/// A tile's features collected from the sort reader, ready for parallel encoding.
struct PendingTile {
    tile_id: u64,
    features: Vec<(u8, Box<[u8]>)>, // (layer_idx, feature_data)
}
const _: () = assert!(std::mem::size_of::<PendingTile>() == 32);

/// An encoded + gzip-compressed tile ready for writing to PMTiles.
struct EncodedTile {
    tile_id: u64,
    compressed: Vec<u8>,
}
const _: () = assert!(std::mem::size_of::<EncodedTile>() == 32);

#[allow(clippy::too_many_lines)]
#[hotpath::measure]
fn phase_assemble(
    sort_reader: &mut sort::SortReader,
    config: &TilegenConfig,
) -> Result<(u64, u64, u64, usize, pmtiles_writer::DedupStats, TileSizeDiagnostics), PipelineError> {
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
        TilePayloadFormat::Mlt => pmtiles.set_tile_contract(TileDataFormat::Mlt, TileDataCompression::None),
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
    let scope_result: Result<_, PipelineError> = std::thread::scope(|s| {
        // --- Reader thread: k-way merge → PendingTile batches ---
        let reader = s.spawn(move || -> Result<(u64, usize), PipelineError> {
            let mut features_read: u64 = 0;
            let mut batch: Vec<PendingTile> = Vec::with_capacity(BATCH_SIZE);
            let mut current = PendingTile { tile_id: u64::MAX, features: Vec::new() };
            let ocean_idx = Layer::Ocean as u8;

            // Incremental byte tracking for assemble batch HWM.
            let mut current_tile_bytes: usize = 0;
            let mut batch_bytes: usize = 0;
            let mut max_batch_bytes: usize = 0;

            // Skip tiles that contain ONLY ocean features (no PBF data).
            // Tilemaker doesn't emit ocean-only tiles; map clients render
            // absent tiles as background. Skipping these cuts tile count by ~5x.
            // Tracked incrementally via has_non_ocean flag instead of scanning
            // all features at tile boundary.
            let mut has_non_ocean = false;

            loop {
                let record = sort_reader.next()?;
                let Some(r) = record else {
                    if current.tile_id != u64::MAX && has_non_ocean {
                        batch_bytes += 32 + current_tile_bytes;
                        batch.push(current);
                    }
                    if !batch.is_empty() {
                        if batch_bytes > max_batch_bytes { max_batch_bytes = batch_bytes; }
                        drop(read_tx.send(batch)); // ignore: encoder may have exited
                    }
                    break;
                };
                features_read += 1;

                let tile_id = sort::tile_id_from_key(r.key);
                let layer_idx = sort::layer_from_key(r.key);

                if tile_id != current.tile_id {
                    if current.tile_id != u64::MAX && has_non_ocean {
                        batch_bytes += 32 + current_tile_bytes;
                        batch.push(current);
                        if batch.len() >= BATCH_SIZE || batch_bytes >= assemble_budget {
                            if batch_bytes > max_batch_bytes { max_batch_bytes = batch_bytes; }
                            if read_tx.send(batch).is_err() { break; }
                            batch = Vec::with_capacity(BATCH_SIZE);
                            batch_bytes = 0;
                        }
                    }
                    current = PendingTile { tile_id, features: Vec::new() };
                    current_tile_bytes = 0;
                    has_non_ocean = false;
                }
                if layer_idx != ocean_idx { has_non_ocean = true; }
                let data_len = r.data.len();
                current.features.push((layer_idx, r.data));
                current_tile_bytes += 32 + data_len;
            }
            Ok((features_read, max_batch_bytes))
        });

        // --- Writer thread: encoded tiles → PMTiles ---
        // move takes ownership of pmtiles; returned via join handle for write_to().
        let writer = s.spawn(move || -> (u64, PmtilesWriter, [u64; 15], [u64; 15], [u64; 15], TileSizeDiagnostics) {
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
                    let is_unique = pmtiles.add_tile(z, x, y, &tile.compressed)
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
            (tiles_written, pmtiles, tiles_per_zoom, unique_per_zoom, bytes_per_zoom, size_diag)
        });

        // --- Main thread: receive batches, encode with rayon, forward to writer ---
        let compression_level = config.compression_level;
        let tile_format = config.tile_format;
        let tile_compression = config.tile_compression;
        for batch in read_rx {
            let encoded = encode_tile_batch(&batch, compression_level, tile_format, tile_compression, &seam_metrics)?;
            if encode_tx.send(encoded).is_err() { break; }
        }
        drop(encode_tx);

        let (features_read, max_batch_bytes) = reader.join().expect("reader panicked")?;
        let (tiles_written, pmtiles, tiles_per_zoom, unique_per_zoom, bytes_per_zoom, size_diag) = writer.join().expect("writer panicked");
        Ok((features_read, tiles_written, pmtiles, tiles_per_zoom, unique_per_zoom, bytes_per_zoom, max_batch_bytes, size_diag))
    });

    let (features_read, tiles_written, mut pmtiles, tiles_per_zoom, unique_per_zoom, bytes_per_zoom, max_batch_bytes, size_diag) = scope_result?;
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
        eprintln!("  Seam reconciliation (boundaries, z<={}): {} tiles, {} rings, {} chains ({} reconciled, {} skipped), {:.1} ms",
            BOUNDARY_NO_SIMP_MAX,
            seam_touched,
            seam_metrics.rings_decoded.load(Ordering::Relaxed),
            seam_metrics.chains_detected.load(Ordering::Relaxed),
            seam_metrics.chains_reconciled.load(Ordering::Relaxed),
            seam_metrics.chains_skipped.load(Ordering::Relaxed),
            seam_metrics.reconcile_us.load(Ordering::Relaxed) as f64 / 1000.0,
        );
    }

    Ok((features_read, tiles_written, unique_tiles, max_batch_bytes, dedup_stats, size_diag))
}

/// Per-worker assembly state, persisted across batches via `thread_local!`.
/// Avoids re-creating Compressor + pools on every batch boundary and keeps
/// LayerBuilder HashMap capacity alive across tiles.
struct AssemblyScratch {
    encode_scratch: mvt::EncodeScratch,
    merge_scratch: mvt::MergeScratch,
    line_merge_scratch: mvt::LineMergeScratch,
    geom_pool: Vec<Vec<u32>>,
    tags_pool: Vec<Vec<(u16, u16)>>,
    compression_levels: [Option<flate2::Compression>; 11],
    gz_buf: Vec<u8>,
    mvt_buf: Vec<u8>,
    layers: [Option<LayerBuilder>; LAYER_COUNT],
    // Shared-edge reconciliation scratch (boundary polygons at z <= BOUNDARY_NO_SIMP_MAX).
    seam_rings: Vec<Vec<(i32, i32)>>,
    /// (feature_index_in_layer, ring_count) — maps decoded rings back to features.
    seam_provenance: Vec<(usize, usize)>,
    seam_encode_buf: Vec<u32>,
}

/// Metrics for shared-edge reconciliation in the assemble phase.
struct SeamMetrics {
    tiles_touched: AtomicU64,
    rings_decoded: AtomicU64,
    chains_detected: AtomicU64,
    chains_reconciled: AtomicU64,
    chains_skipped: AtomicU64,
    reconcile_us: AtomicU64,
}

impl SeamMetrics {
    fn new() -> Self {
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
fn reconcile_boundary_seams(
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
        let valid_count = decoded.iter().filter(|r| r.len() >= 4 && r.first() == r.last()).count();
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
        metrics.reconcile_us.fetch_add(elapsed_us, Ordering::Relaxed);
        return;
    }

    metrics.tiles_touched.fetch_add(1, Ordering::Relaxed);
    metrics.rings_decoded.fetch_add(seam_rings.len() as u64, Ordering::Relaxed);

    // Detect shared chains (needs >= 2 rings to find any).
    let chains = if seam_rings.len() >= 2 {
        geometry::detect_shared_chains(seam_rings)
    } else {
        Vec::new()
    };
    metrics.chains_detected.fetch_add(chains.len() as u64, Ordering::Relaxed);

    // Canonicalize: copy first incident's vertices to second incident's ring.
    if !chains.is_empty() {
        let canon_result = geometry::canonicalize_shared_chains(seam_rings, &chains);
        metrics.chains_reconciled.fetch_add(canon_result.reconciled as u64, Ordering::Relaxed);
        metrics.chains_skipped.fetch_add(canon_result.skipped as u64, Ordering::Relaxed);
    }

    // Tile-coordinate DP on all rings, pinning shared-chain vertices.
    // Runs even when no chains were found — these rings skipped PBF-phase DP
    // and need tile-coord simplification regardless.
    for (ring_idx, ring) in seam_rings.iter_mut().enumerate() {
        let pinned = geometry::build_pinned_mask(ring.len(), ring_idx, &chains);
        let simplified = geometry::simplify_ring_tile_coords(ring, &pinned, geometry::TILE_SIMPLIFY_TOLERANCE);
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
    metrics.reconcile_us.fetch_add(elapsed_us, Ordering::Relaxed);
}

/// Encode + compress a batch of tiles in parallel using rayon.
#[hotpath::measure]
#[allow(clippy::cast_possible_wrap)]
fn encode_tile_batch(
    batch: &[PendingTile],
    compression_level: u32,
    tile_format: TilePayloadFormat,
    tile_compression: TileCompression,
    seam_metrics: &SeamMetrics,
) -> Result<Vec<EncodedTile>, PipelineError> {
    match tile_format {
        TilePayloadFormat::Mvt => Ok(encode_tile_batch_mvt(batch, compression_level, tile_compression, seam_metrics)),
        TilePayloadFormat::Mlt => encode_tile_batch_mlt(batch),
    }
}

/// Encode + compress a batch of MVT tiles in parallel using rayon.
#[hotpath::measure]
#[allow(clippy::cast_possible_wrap)]
fn encode_tile_batch_mvt(batch: &[PendingTile], compression_level: u32, tile_compression: TileCompression, seam_metrics: &SeamMetrics) -> Vec<EncodedTile> {
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

            // Shared-edge reconciliation for boundary polygons at low zoom.
            // Must run BEFORE merge_same_attr_geometries (which destroys per-ring identity).
            let boundary_layer_idx = Layer::Boundaries as usize;
            if z <= BOUNDARY_NO_SIMP_MAX
                && let Some(lb) = s.layers[boundary_layer_idx].as_mut()
            {
                reconcile_boundary_seams(lb, &mut s.seam_rings, &mut s.seam_provenance, &mut s.seam_encode_buf, seam_metrics);
            }

            // Merge same-attribute geometries to reduce feature count
            for layer in &mut s.layers {
                if let Some(lb) = layer.as_mut() {
                    lb.merge_same_attr_geometries(&mut s.merge_scratch, &mut s.geom_pool, &mut s.tags_pool);
                }
            }

            // Merge connected line segments through degree-2 nodes.
            // Skip at z14 where lines are full resolution and merging adds
            // overhead without meaningful compression benefit.
            if z < 14 {
                for layer in &mut s.layers {
                    if let Some(lb) = layer.as_mut() {
                        lb.merge_connected_lines(&mut s.line_merge_scratch);
                    }
                }
            }

            // Max 26 elements (one per Shortbread layer) — with_capacity not needed.
            let non_empty: Vec<&LayerBuilder> = s.layers.iter()
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
                    let lvl = *s.compression_levels[level].get_or_insert_with(|| {
                        flate2::Compression::new(level as u32)
                    });
                    let mut encoder = flate2::write::GzEncoder::new(compress_buf, lvl);
                    std::io::Write::write_all(&mut encoder, &s.mvt_buf)
                        .expect("gzip compress failed");
                    encoder.finish().expect("gzip finish failed")
                }
                TileCompression::Brotli => {
                    #[allow(clippy::cast_possible_truncation)]
                    let quality = level as u32;
                    let mut encoder = brotli::CompressorWriter::new(
                        &mut compress_buf, 4096, quality, 22,
                    );
                    std::io::Write::write_all(&mut encoder, &s.mvt_buf)
                        .expect("brotli compress failed");
                    drop(encoder);
                    compress_buf
                }
            };
            s.gz_buf = Vec::with_capacity(compressed.len());

            Some(EncodedTile { tile_id: tile.tile_id, compressed })
            })
        })
        .flatten()
        .collect()
}

/// Build non-empty per-layer builders for one tile.
fn prepare_non_empty_layers<'a>(
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

    // Merge same-attribute geometries to reduce feature count.
    for layer in &mut s.layers {
        if let Some(lb) = layer.as_mut() {
            lb.merge_same_attr_geometries(&mut s.merge_scratch, &mut s.geom_pool, &mut s.tags_pool);
        }
    }

    // Max 26 elements (one per Shortbread layer) — with_capacity not needed.
    s.layers
        .iter()
        .filter_map(|l| l.as_ref())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Encode an MLT batch (currently scaffolded, returns not-implemented error with tile context).
#[hotpath::measure]
fn encode_tile_batch_mlt(batch: &[PendingTile]) -> Result<Vec<EncodedTile>, PipelineError> {
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

const LAYER_COUNT: usize = Layer::count();

/// Get or create a LayerBuilder at the given index.
fn get_or_create_layer(layers: &mut [Option<LayerBuilder>], idx: usize) -> &mut LayerBuilder {
    if layers[idx].is_none() {
        layers[idx] = Some(LayerBuilder::new(Layer::ALL[idx].name()));
    }
    layers[idx].as_mut().expect("just inserted")
}


// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn centroid_of(points: &[Point]) -> Point {
    if points.is_empty() {
        return Point { x: 0.0, y: 0.0 };
    }
    let mut sx = 0.0;
    let mut sy = 0.0;
    for p in points {
        sx += p.x;
        sy += p.y;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = points.len() as f64;
    Point { x: sx / n, y: sy / n }
}

// ---------------------------------------------------------------------------
// Tests (see pipeline_tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pipeline_tests.rs"]
mod tests;
