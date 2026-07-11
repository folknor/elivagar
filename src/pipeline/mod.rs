// Tile generation pipeline orchestrator.
//
// Reads an OSM PBF file and produces a Shortbread-schema PMTiles v3 archive.
// Pipeline:
//   Phase 1+2: Single-pass PBF read - build node/way indices AND process features
//   Phase 3:   External merge sort by Hilbert tile ID
//   Phase 4:   Tile assembly (MVT encode + gzip) + PMTiles write

mod assemble;
pub(crate) mod emit;
mod phase12;
mod relations;
mod stats;

#[cfg(test)]
use crate::geometry;
use crate::geometry::MercBbox;
use crate::pmtiles_writer;
use crate::shortbread;
use crate::sort;

use std::path::PathBuf;

use crate::debug::{
    WAIT, emit_alloc_boundary, emit_counter, emit_counter_u64, emit_counter_usize, emit_marker,
    emit_wait_counters, wait_span,
};
use std::sync::atomic::Ordering;
use std::time::Instant;

use stats::Phase12Stats;
#[cfg(test)]
use stats::missing_ref_summary_lines;

/// Pipeline error type. Stringly-typed because no caller inspects variants -
/// errors are only displayed or propagated. An enum would add boilerplate for no benefit.
#[derive(Debug)]
pub struct PipelineError(pub String);

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PipelineError {}

// Intentionally converts to String - no caller inspects .source() programmatically.
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
    /// Run the global shared-node prepass and pin cross-block shared way
    /// vertices during line and polygon simplification. Disabled by default:
    /// block-local pins preserve common junctions within each PBF primitive
    /// block while avoiding a second full way scan and a large long-lived set.
    /// Enabling this preserves more low-zoom detail on very thin linear water
    /// and polygon edges, at a substantial runtime and RSS cost.
    pub global_shared_node_pins: bool,
    /// Tile payload format (`mvt` default, `mlt` planned).
    pub tile_format: TilePayloadFormat,
    /// Tile compression algorithm for MVT payloads (`gzip` default, `brotli` optional).
    pub tile_compression: TileCompression,
    /// Compression algorithm for sort chunk files. Reduces disk I/O at the
    /// cost of CPU. Useful at planet scale where sort data exceeds available RAM.
    pub compress_sort_chunks: sort::ChunkCompression,
    /// Per-layer max zoom for shared-edge seam reconciliation.
    /// 0 = disabled, 1-14 = max zoom at which to defer simplification.
    /// Indexed by Layer enum discriminant. Default: Boundaries=8, rest=0.
    pub seam_reconcile_layers: [u8; shortbread::Layer::count()],
    /// Per-layer fanout caps: maximum bbox tiles a polygon feature may touch
    /// at any zoom. When exceeded, the feature is skipped at that zoom.
    /// 0 = uncapped (default). Only applies to polygon-geometry emit functions.
    /// Indexed by `Layer` enum discriminant.
    pub fanout_caps: [u32; shortbread::Layer::count()],
    /// Simplification tolerance multiplier for polygon layers. 1.0 = standard
    /// (same tolerance as lines). Values > 1.0 simplify more aggressively,
    /// reducing sort record volume. Polygon fill rendering is less sensitive
    /// to vertex precision than stroked lines, so higher tolerances are safe.
    /// Expected sweet spot: 1.0-2.0. Default: 1.0 (no change).
    pub polygon_simplify_factor: f64,
}

const CHECKPOINT_FILE: &str = "checkpoint.txt";
const SORT_CHECKPOINT_FILE: &str = "sort_chunks.count";
const SORT_CHUNKS_DIR: &str = "sort_chunks";
/// Default memory budget per sort chunk (1 GB).
const DEFAULT_SORT_CHUNK_SIZE: usize = 1 << 30;

// ---------------------------------------------------------------------------
// Checkpoint I/O
// ---------------------------------------------------------------------------

fn save_checkpoint(
    tmp_dir: &std::path::Path,
    bounds: &MercBbox,
    chunk_count: usize,
) -> Result<(), PipelineError> {
    let path = tmp_dir.join(CHECKPOINT_FILE);
    let content = format!(
        "{} {} {} {} {}",
        bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y, chunk_count
    );
    std::fs::write(path, content)?; // io::Error message is sufficient context.
    Ok(())
}

/// Save total chunk count so --skip-to sort can validate against stale leftovers.
fn save_sort_chunk_count(
    tmp_dir: &std::path::Path,
    count: Option<usize>,
) -> Result<(), PipelineError> {
    if let Some(n) = count {
        std::fs::write(tmp_dir.join(SORT_CHECKPOINT_FILE), n.to_string())?;
    }
    Ok(())
}

fn load_sort_chunk_count(tmp_dir: &std::path::Path) -> Option<usize> {
    match std::fs::read_to_string(tmp_dir.join(SORT_CHECKPOINT_FILE)) {
        Ok(content) => content.trim().parse().ok(),
        Err(_) => {
            eprintln!("  Warning: no sort checkpoint found - cannot verify chunk integrity");
            None
        }
    }
}

fn load_checkpoint(tmp_dir: &std::path::Path) -> Result<(MercBbox, usize), PipelineError> {
    let path = tmp_dir.join(CHECKPOINT_FILE);
    let content = std::fs::read_to_string(&path).map_err(|e| {
        PipelineError(format!(
            "no checkpoint in {}: {e} (run a full tilegen first)",
            tmp_dir.display()
        ))
    })?;
    let parts: Vec<&str> = content.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(PipelineError(format!(
            "invalid checkpoint format: expected 5 fields, got {}",
            parts.len()
        )));
    }
    let parse = |s: &str, name: &str| -> Result<f64, PipelineError> {
        s.parse()
            .map_err(|e| PipelineError(format!("checkpoint parse {name}: {e}")))
    };
    let bounds = MercBbox {
        min_x: parse(parts[0], "min_x")?,
        min_y: parse(parts[1], "min_y")?,
        max_x: parse(parts[2], "max_x")?,
        max_y: parse(parts[3], "max_y")?,
    };
    let chunks: usize = parts[4]
        .parse()
        .map_err(|e| PipelineError(format!("checkpoint parse chunk count: {e}")))?;
    Ok((bounds, chunks))
}

// ---------------------------------------------------------------------------
// RSS helpers
// ---------------------------------------------------------------------------

/// Read peak resident set size (VmHWM) from `/proc/self/status`.
/// Returns `None` on non-Linux platforms or if parsing fails.
///
/// NOTE: VmHWM is the process-lifetime high-water mark - it never decreases.
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

fn emit_allocator_boundary(name: &str) {
    emit_alloc_boundary(name);
}

/// The per-layer per-zoom sort stats (records, bytes, fanout percentiles,
/// threshold counts) are a ~800-counter firehose - roughly 26 layers x 15 zooms
/// x several metrics - that swamps `brokkr sidecar --counters` and floods an
/// optimizer's context. They are diagnostic detail wanted only during layer or
/// fanout-cap analysis, so they are emitted only when ELIVAGAR_LAYER_STATS is
/// set (mirroring ELIVAGAR_NODE_STATS). The per-layer totals
/// (`sort_layer_<name>_records`/`_bytes`) are always emitted.
fn layer_stats_enabled() -> bool {
    std::env::var_os("ELIVAGAR_LAYER_STATS").is_some()
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

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
    emit_allocator_boundary("run_start");

    eprintln!(
        "=== Tilegen: {} → {}",
        config.pbf_path.display(),
        config.output_path.display()
    );
    eprintln!("    Zoom range: z{}-z{}", config.min_zoom, config.max_zoom);
    eprintln!("    Tmp dir:    {}", config.tmp_dir.display());
    eprintln!(
        "    Tile format:{:?}, compression:{:?}",
        config.tile_format, config.tile_compression
    );
    if (config.polygon_simplify_factor - 1.0).abs() > f64::EPSILON {
        eprintln!(
            "    Polygon simplify factor: {:.2}",
            config.polygon_simplify_factor
        );
    }
    if let Some(s) = skip {
        eprintln!("    Skip to:    {s:?}");
    }

    // --- Phase 1+2: PBF read + feature processing ---
    emit_marker("PHASE12_START");
    emit_allocator_boundary("phase12_start");
    let ocean_elapsed;
    let mut phase12_rss: Option<u64> = None;
    let mut ocean_rss: Option<u64> = None;
    let mut phase12_stats: Option<Phase12Stats> = None;

    let mut sort_writer = if matches!(skip, Some(SkipTo::Sort | SkipTo::Assemble)) {
        // Skip straight to later phases - reuse existing chunks on disk.
        ocean_elapsed = None;
        if skip == Some(SkipTo::Sort) {
            eprintln!("--- Skipping to sort (using existing chunks) ---");
        } else {
            eprintln!("--- Skipping to assemble (using existing chunks) ---");
        }
        None
    } else {
        let mut sort_writer = if skip.is_none() {
            // Full run: clean tmp dir and run PBF phase
            drop(std::fs::remove_dir_all(&config.tmp_dir)); // Best-effort: may not exist yet.
            std::fs::create_dir_all(&config.tmp_dir)?; // io::Error message is sufficient context.

            let phase12_start = Instant::now();
            let (mut sw, bounds_out, p12_stats) = phase12::phase_read_and_process(config)?;
            phase12_stats = Some(p12_stats);
            let phase12_elapsed = phase12_start.elapsed();
            emit_marker("PHASE12_END");
            emit_counter(
                "phase12_ms",
                i64::try_from(phase12_elapsed.as_millis()).unwrap_or(i64::MAX),
            );
            phase12_rss = peak_rss_kb();
            {
                let _wait = wait_span(&WAIT.sort_flush);
                sw.flush()?; // Flush buffer so chunk_count() is accurate for checkpoint
            }
            emit_allocator_boundary("phase12_end");
            save_checkpoint(&config.tmp_dir, &bounds_out, sw.chunk_count())?;
            sw
        } else {
            // --skip-to ocean: load checkpoint, resume from PBF chunks
            let (_, pbf_chunks) = load_checkpoint(&config.tmp_dir)?;
            eprintln!("--- Skipping PBF phase ({pbf_chunks} chunks from checkpoint) ---");
            sort::SortWriter::resume(
                &config.tmp_dir.join(SORT_CHUNKS_DIR),
                sort_chunk_size,
                pbf_chunks,
                config.compress_sort_chunks,
            )?
        };

        // Load data_bounds (needed for ocean, always available from checkpoint or just computed)
        let (data_bounds, _) = load_checkpoint(&config.tmp_dir)?;

        // --- Ocean shapefile processing ---
        // When a simplified shapefile is provided, use it for z0-7 and the
        // full-resolution shapefile for z8+. Otherwise use the full-res for all zooms.
        emit_marker("OCEAN_START");
        emit_allocator_boundary("ocean_start");
        ocean_elapsed = if let Some(ref ocean_path) = config.ocean_shapefile {
            let ocean_start = Instant::now();
            eprintln!("--- Ocean shapefile ---");
            let mut ocean_features: u64 = 0;

            if let Some(ref simplified_path) = config.ocean_simplified_shapefile {
                let simplified_max = config.max_zoom.min(7);
                if config.min_zoom <= simplified_max {
                    eprintln!("  Simplified (z{}-z{}):", config.min_zoom, simplified_max);
                    ocean_features += crate::ocean::process_ocean_shapefile(
                        simplified_path,
                        &data_bounds,
                        config.min_zoom,
                        simplified_max,
                        &mut sort_writer,
                    )?;
                }
                if config.max_zoom >= 8 {
                    let full_min = config.min_zoom.max(8);
                    eprintln!("  Full-resolution (z{full_min}-z{}):", config.max_zoom);
                    ocean_features += crate::ocean::process_ocean_shapefile(
                        ocean_path,
                        &data_bounds,
                        full_min,
                        config.max_zoom,
                        &mut sort_writer,
                    )?;
                }
            } else {
                ocean_features = crate::ocean::process_ocean_shapefile(
                    ocean_path,
                    &data_bounds,
                    config.min_zoom,
                    config.max_zoom,
                    &mut sort_writer,
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

    emit_marker("OCEAN_END");
    emit_allocator_boundary("ocean_end");
    crate::ocean::emit_ocean_counters();
    if let Some((elapsed, features)) = ocean_elapsed {
        emit_counter(
            "ocean_ms",
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        );
        emit_counter(
            "ocean_features",
            i64::try_from(features).unwrap_or(i64::MAX),
        );
    }

    // --- Phase 3: Sort ---
    emit_marker("SORT_START");
    emit_allocator_boundary("sort_start");
    // Flush any trailing buffer so chunk_count() reflects all chunks on disk,
    // then save the count for --skip-to sort validation.
    if let Some(ref mut sw) = sort_writer {
        let _wait = wait_span(&WAIT.sort_flush);
        sw.flush()?;
    }
    let sort_chunks = sort_writer.as_ref().map(sort::SortWriter::chunk_count);
    save_sort_chunk_count(&config.tmp_dir, sort_chunks)?;
    let (mut sort_reader, phase3_elapsed, sort_rss) = if skip == Some(SkipTo::Assemble) {
        let sr = {
            let _wait = wait_span(&WAIT.sort_open);
            sort::SortReader::from_dir(
                &config.tmp_dir.join(SORT_CHUNKS_DIR),
                load_sort_chunk_count(&config.tmp_dir),
                config.compress_sort_chunks,
            )?
        };
        (sr, None, peak_rss_kb())
    } else {
        let phase3_start = Instant::now();
        eprintln!("--- Sort ---");
        let sr = if let Some(sw) = sort_writer {
            let _wait = wait_span(&WAIT.sort_finish);
            sw.finish()?
        } else {
            let _wait = wait_span(&WAIT.sort_open);
            sort::SortReader::from_dir(
                &config.tmp_dir.join(SORT_CHUNKS_DIR),
                load_sort_chunk_count(&config.tmp_dir),
                config.compress_sort_chunks,
            )?
        };
        (sr, Some(phase3_start.elapsed()), peak_rss_kb())
    };

    emit_marker("SORT_END");
    emit_allocator_boundary("sort_end");

    // --- Phase 4: Tile assembly + PMTiles write ---
    emit_marker("ASSEMBLE_START");
    emit_allocator_boundary("assemble_start");
    let phase4_start = Instant::now();
    eprintln!("--- Tile assembly ---");
    let (
        features_read,
        tiles_written,
        unique_tiles,
        max_assemble_batch_bytes,
        dedup_stats,
        tile_size_diag,
    ) = assemble::phase_assemble(&mut sort_reader, config)?;
    // Drop the reader so every chunk reader flushes its byte tally before we
    // emit the merge counters.
    drop(sort_reader);
    sort::emit_sort_counters();
    let phase4_elapsed = phase4_start.elapsed();
    emit_marker("ASSEMBLE_END");
    emit_allocator_boundary("assemble_end");
    emit_counter(
        "assemble_ms",
        i64::try_from(phase4_elapsed.as_millis()).unwrap_or(i64::MAX),
    );
    emit_counter("tiles", i64::try_from(tiles_written).unwrap_or(i64::MAX));
    emit_counter(
        "unique_tiles",
        i64::try_from(unique_tiles).unwrap_or(i64::MAX),
    );
    emit_counter("features", i64::try_from(features_read).unwrap_or(i64::MAX));
    let assemble_rss = peak_rss_kb();

    let total = total_start.elapsed();
    emit_allocator_boundary("run_end");

    emit_counter(
        "total_ms",
        i64::try_from(total.as_millis()).unwrap_or(i64::MAX),
    );
    emit_wait_counters();
    // Record the run's tile encoding config as enum-int counters. Counters are
    // i64-only, so these categorical settings would otherwise be unrecorded in
    // the sidecar; they are only otherwise recoverable from cli_args, and then
    // only when the flags were passed explicitly rather than left at defaults.
    emit_counter(
        "tile_format",
        match config.tile_format {
            TilePayloadFormat::Mvt => 0,
            TilePayloadFormat::Mlt => 1,
        },
    );
    emit_counter(
        "tile_compression",
        match config.tile_compression {
            TileCompression::Gzip => 0,
            TileCompression::Brotli => 1,
        },
    );
    if let Some(p3) = phase3_elapsed {
        emit_counter(
            "phase3_ms",
            i64::try_from(p3.as_millis()).unwrap_or(i64::MAX),
        );
    }
    if let Ok(meta) = std::fs::metadata(&config.output_path) {
        emit_counter_u64("output_bytes", meta.len());
    }
    if let Some(ref s) = phase12_stats
        && let Some((nodes, groups)) = s.node_store_stats
    {
        emit_counter_u64("node_store_nodes", nodes);
        emit_counter_usize("node_store_groups", groups);
    }
    if let Some(n) = sort_chunks {
        emit_counter_usize("sort_chunks", n);
    }
    if let Some(ref s) = phase12_stats {
        emit_counter_u64("sort_records", s.sort_records);
        emit_counter_u64("sort_record_bytes", s.sort_record_bytes);
        emit_counter_u64("phase12_nodes", s.node_count);
        emit_counter_u64("phase12_ways", s.way_count);
        emit_counter_u64("phase12_relations", s.rel_count);
        if let Some(records_per_way_x10) =
            s.sort_records.saturating_mul(10).checked_div(s.way_count)
        {
            emit_counter_u64("records_per_way_x10", records_per_way_x10);
        }
        let layer_stats = layer_stats_enabled();
        for (i, (&recs, &bytes)) in s.layer_records.iter().zip(s.layer_bytes.iter()).enumerate() {
            if recs > 0 && i < shortbread::Layer::ALL.len() {
                let name = shortbread::Layer::ALL[i].name();
                emit_counter_u64(&format!("sort_layer_{name}_records"), recs);
                emit_counter_u64(&format!("sort_layer_{name}_bytes"), bytes);
                if !layer_stats {
                    continue;
                }
                for z in 0..15u8 {
                    let idx = i * 15 + z as usize;
                    let zr = s.layer_zoom_records[idx];
                    let zb = s.layer_zoom_bytes[idx];
                    if zr > 0 {
                        emit_counter_u64(&format!("sort_layer_{name}_z{z}_records"), zr);
                    }
                    if zb > 0 {
                        emit_counter_u64(&format!("sort_layer_{name}_z{z}_bytes"), zb);
                    }
                    let max = s.fanout_stats.max_tiles[idx];
                    if max > 0 {
                        emit_counter_u64(
                            &format!("sort_layer_{name}_z{z}_fanout_p50"),
                            u64::from(s.fanout_stats.percentile(i, z as usize, 0.50)),
                        );
                        emit_counter_u64(
                            &format!("sort_layer_{name}_z{z}_fanout_p95"),
                            u64::from(s.fanout_stats.percentile(i, z as usize, 0.95)),
                        );
                        emit_counter_u64(
                            &format!("sort_layer_{name}_z{z}_fanout_p99"),
                            u64::from(s.fanout_stats.percentile(i, z as usize, 0.99)),
                        );
                        emit_counter_u64(
                            &format!("sort_layer_{name}_z{z}_fanout_max"),
                            u64::from(max),
                        );
                    }
                    let cf = s.fanout_stats.capped_features[idx];
                    let ct = s.fanout_stats.capped_tiles[idx];
                    if cf > 0 {
                        emit_counter_u64(&format!("fanout_capped_features_{name}_z{z}"), cf);
                        emit_counter_u64(&format!("fanout_capped_tiles_{name}_z{z}"), ct);
                        let avg_bytes = zb.checked_div(zr).unwrap_or(0);
                        emit_counter_u64(
                            &format!("fanout_capped_bytes_estimated_{name}_z{z}"),
                            ct.saturating_mul(avg_bytes),
                        );
                    }
                }
                for &thresh in &[128u32, 512, 2048] {
                    for z in 0..15u8 {
                        let n = s.fanout_stats.features_above(i, z as usize, thresh);
                        if n > 0 {
                            emit_counter_u64(&format!("sort_layer_{name}_z{z}_above_{thresh}"), n);
                        }
                    }
                }
            }
        }
        for (rank, &(osm_id, layer, zoom, bbox_tiles)) in
            s.fanout_stats.top_capped.iter().enumerate()
        {
            let prefix = format!("fanout_capped_top_{}", rank + 1);
            emit_counter_u64(&format!("{prefix}_osm_id"), osm_id);
            emit_counter_u64(&format!("{prefix}_layer"), u64::from(layer));
            emit_counter_u64(&format!("{prefix}_zoom"), u64::from(zoom));
            emit_counter_u64(&format!("{prefix}_bbox_tiles"), bbox_tiles);
        }
        for (i, max_z) in config.seam_reconcile_layers.iter().enumerate() {
            if *max_z > 0 {
                let verts = s.deferral_stats.vertices[i].load(Ordering::Relaxed);
                if verts > 0 {
                    let name = shortbread::Layer::ALL[i].name();
                    emit_counter_u64(&format!("seam_deferred_vertices_{name}"), verts);
                    if s.deferral_stats.disabled[i].load(Ordering::Relaxed) {
                        emit_counter(&format!("seam_deferral_disabled_{name}"), 1);
                    }
                }
            }
        }
    }
    if let Some(kb) = phase12_rss {
        emit_counter_u64("phase12_rss_kb", kb);
    }
    if let Some(kb) = ocean_rss {
        emit_counter_u64("ocean_rss_kb", kb);
    }
    if let Some(kb) = sort_rss {
        emit_counter_u64("sort_rss_kb", kb);
    }
    if let Some(kb) = assemble_rss {
        emit_counter_u64("assemble_rss_kb", kb);
    }
    let peak_rss = [phase12_rss, ocean_rss, sort_rss, assemble_rss]
        .iter()
        .filter_map(|v| *v)
        .max();
    if let Some(kb) = peak_rss {
        emit_counter_u64("peak_rss_kb", kb);
    }
    if let Some(ref s) = phase12_stats {
        emit_counter_usize("max_way_inflight_bytes", s.max_way_inflight_bytes);
        emit_counter_usize("max_rel_inflight_bytes", s.max_rel_inflight_bytes);
        emit_counter_usize("relation_blocks_buffered", s.relation_blocks_buffered);
        emit_counter_usize("relation_blocks_bytes", s.relation_blocks_bytes);
        emit_counter_usize(
            "relation_blocks_spilled",
            usize::from(s.relation_blocks_spilled),
        );
        emit_counter_usize("relation_plan_needed_ways", s.relation_plan_needed_ways);
        emit_counter_usize("relation_plan_superset_ways", s.relation_plan_superset_ways);
        emit_counter_u64("way_members_marked", s.way_members_marked);
        emit_counter_usize("global_shared_nodes", s.global_shared_nodes);
        if let Some(kb) = s.relation_blocks_drop_rss_kb {
            emit_counter_u64("relation_blocks_drop_rss_kb", kb);
        }
        emit_counter_u64(
            "missing_way_node_refs",
            s.missing_refs.missing_way_node_refs,
        );
        emit_counter_u64(
            "ways_with_missing_node_refs",
            s.missing_refs.ways_with_missing_node_refs,
        );
        emit_counter_u64(
            "missing_relation_way_refs",
            s.missing_refs.missing_relation_way_refs,
        );
        emit_counter_u64(
            "relations_with_missing_way_refs",
            s.missing_refs.relations_with_missing_way_refs,
        );
        emit_counter_u64(
            "relation_non_way_members",
            s.missing_refs.relation_non_way_members,
        );
        emit_counter_u64(
            "relation_nested_members",
            s.missing_refs.relation_nested_members,
        );
    }
    emit_counter_usize("max_assemble_batch_bytes", max_assemble_batch_bytes);
    emit_counter_u64("dedup_candidates", dedup_stats.candidates);
    emit_counter_u64("dedup_tiles_reused", dedup_stats.tiles_reused);
    emit_counter_u64("dedup_bytes_saved", dedup_stats.bytes_saved);
    emit_counter_u64("dedup_reject_len_mismatch", dedup_stats.reject_len_mismatch);
    emit_counter_u64("dedup_reject_fp_mismatch", dedup_stats.reject_fp_mismatch);
    emit_counter_u64("dedup_insert_skipped_cap", dedup_stats.insert_skipped_cap);
    emit_counter_u64(
        "dedup_hash_bucket_collisions",
        dedup_stats.hash_bucket_collisions,
    );
    emit_counter_u64("tile_bytes_total", tile_size_diag.total_tile_bytes);
    emit_counter_u64(
        "tile_bytes_avg",
        tile_size_diag
            .total_tile_bytes
            .checked_div(tiles_written)
            .unwrap_or(0),
    );
    emit_counter_u64("tile_max_bytes", tile_size_diag.max_tile.bytes);
    if tile_size_diag.max_tile.bytes > 0 {
        let (z, x, y) = pmtiles_writer::tile_id_to_zxy(tile_size_diag.max_tile.tile_id);
        emit_counter_u64("tile_max_z", u64::from(z));
        emit_counter_u64("tile_max_x", u64::from(x));
        emit_counter_u64("tile_max_y", u64::from(y));
    }
    emit_counter_u64("oversize_tiles_warn", tile_size_diag.oversize_warn_count);
    emit_counter_u64(
        "oversize_tiles_severe",
        tile_size_diag.oversize_severe_count,
    );
    for (i, t) in tile_size_diag.top_oversized.iter().enumerate() {
        if t.bytes == 0 {
            continue;
        }
        let (z, x, y) = pmtiles_writer::tile_id_to_zxy(t.tile_id);
        let prefix = format!("oversize_top_{}", i + 1);
        emit_counter_u64(&format!("{prefix}_z"), u64::from(z));
        emit_counter_u64(&format!("{prefix}_x"), u64::from(x));
        emit_counter_u64(&format!("{prefix}_y"), u64::from(y));
        emit_counter_u64(&format!("{prefix}_bytes"), t.bytes);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (see pipeline_tests.rs)
// ---------------------------------------------------------------------------

// Re-export submodule items so `use super::*` in pipeline_tests.rs works.
#[cfg(test)]
use crate::geometry::Point;
#[cfg(test)]
use crate::geometry::merc_bbox;
#[cfg(test)]
use crate::mlt;
#[cfg(test)]
use crate::multipolygon::{MemberWay, WayRole};
#[cfg(test)]
use crate::mvt::GeomType;
#[cfg(test)]
use crate::sort::SortRecord;
#[cfg(test)]
use assemble::{
    AssemblyScratch, LAYER_COUNT, PendingTile, SeamMetrics, encode_tile_batch,
    encode_tile_batch_mvt, phase_assemble, prepare_non_empty_layers,
};
#[cfg(test)]
use emit::{
    LineEmitScratch, MultipolygonEmitScratch, PointEmitScratch, PolygonEmitScratch,
    antimeridian_shifts_for_bbox, emit_line_feature, emit_multipolygon_feature,
    emit_point_or_centroid, emit_polygon_feature, enrich_polygon_matches, merc_point_key,
    relation_shared_vertex_keys, unwrap_antimeridian_path,
};
#[cfg(test)]
use phase12::{
    MembersForBlock, NodeStoreMode, RawWay, annotate_block_shared_node_refs, build_way_plans,
    crosses_antimeridian, lon_e7_shifted_360, phase_read_and_process, select_node_store_mode,
};
#[cfg(test)]
use stats::{
    DEFERRAL_VERTEX_BUDGET, DeferralStats, MissingRefStats, MissingRefStatsAtomic, OversizeTile,
    TILE_OVERSIZE_SEVERE_BYTES, TILE_OVERSIZE_TOP_N, TILE_OVERSIZE_WARN_BYTES, TileSizeDiagnostics,
    insert_top_oversized, record_tile_size_diagnostics,
};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../pipeline_tests.rs"]
mod tests;
