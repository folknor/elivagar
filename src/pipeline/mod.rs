// Tile generation pipeline orchestrator.
//
// Reads an OSM PBF file and produces a Shortbread-schema PMTiles v3 archive.
// Pipeline:
//   Phase 1+2: Single-pass PBF read - build node/way indices AND process features
//   Phase 3:   External merge sort by Hilbert tile ID
//   Phase 4:   Tile assembly (MVT encode + gzip) + PMTiles write

mod assemble;
mod boundary_prescan;
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

use std::path::{Path, PathBuf};

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

/// Ocean producer selected for the checkpointed chunk set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OceanMode {
    None,
    Computed,
    Band { key: crate::ocean::OceanArtifactKey },
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
    /// Optional durable world-ocean PMTiles artifact. It activates only for
    /// the exact MVT/gzip z0-14 contract; every other configuration keeps the
    /// computed shapefile path.
    pub ocean_tiles: Option<PathBuf>,
    /// Metadata key written by the ocean-only artifact builder.
    pub ocean_artifact_key: Option<crate::ocean::OceanArtifactKey>,
    /// Restrict PMTiles metadata to the ocean layer for an ocean artifact.
    pub ocean_only_metadata: bool,
    /// Disable ocean simplification for a same-source coverage baseline.
    pub no_ocean_simplify: bool,
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
/// v5 is JSON and records everything a resume must verify: the input hash, the
/// producer config the chunks were built under, and the effective paths.
///
/// v3 and earlier stored only bounds, chunk count and ocean mode, so a resume
/// could silently reuse chunks built from a different PBF or a different zoom
/// range. v4 added the input hash and effective paths but kept the
/// whitespace-positional encoding, where every new field shifts an index and a
/// miscount is a silent misparse. Neither is accepted; re-run a full tilegen.
const CHECKPOINT_VERSION: u32 = 5;
const SORT_CHECKPOINT_FILE: &str = "sort_chunks.count";
const SORT_CHUNKS_DIR: &str = "sort_chunks";
/// Default memory budget per sort chunk (1 GB).
const DEFAULT_SORT_CHUNK_SIZE: usize = 1 << 30;

// ---------------------------------------------------------------------------
// Checkpoint I/O
// ---------------------------------------------------------------------------

/// Everything a resumed run must verify or inherit, recorded when the PBF
/// phase produces the chunks.
///
/// Chunks on disk are the product of one PBF and one producer config, and
/// nothing in a later invocation reveals either. Resuming against a different
/// input, or a different zoom range or fanout cap, silently mixes two
/// contracts into one archive and then stamps it with provenance describing
/// only the second - the metadata would be confidently wrong, which is worse
/// than absent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CheckpointProvenance {
    pub(crate) input_xxh3_128: String,
    /// `provenance::producer_config` of the run that wrote the chunks.
    pub(crate) producer_config: serde_json::Value,
    pub(crate) effective: crate::provenance::Effective,
}

fn save_checkpoint(
    tmp_dir: &std::path::Path,
    bounds: &MercBbox,
    chunk_count: usize,
    ocean_mode: &OceanMode,
    provenance: &CheckpointProvenance,
) -> Result<(), PipelineError> {
    let path = tmp_dir.join(CHECKPOINT_FILE);
    let ocean = match ocean_mode {
        OceanMode::None => serde_json::json!("none"),
        OceanMode::Computed => serde_json::json!("computed"),
        OceanMode::Band { key } => serde_json::json!({
            "band": serde_json::from_str::<serde_json::Value>(&key.json())
                .map_err(|e| PipelineError(format!("ocean key is not valid JSON: {e}")))?,
        }),
    };
    let content = serde_json::json!({
        "version": CHECKPOINT_VERSION,
        "bounds": [bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y],
        "chunks": chunk_count,
        "ocean": ocean,
        "input_xxh3_128": provenance.input_xxh3_128,
        "producer_config": provenance.producer_config,
        "effective": {
            "coordinate_source": provenance.effective.coordinate_source,
            "way_members": provenance.effective.way_members,
            "shared_node_pins": provenance.effective.shared_node_pins,
        },
    });
    std::fs::write(path, content.to_string())?; // io::Error message is sufficient context.
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

fn load_checkpoint(
    tmp_dir: &std::path::Path,
) -> Result<(MercBbox, usize, OceanMode, CheckpointProvenance), PipelineError> {
    let path = tmp_dir.join(CHECKPOINT_FILE);
    let content = std::fs::read_to_string(&path).map_err(|e| {
        PipelineError(format!(
            "no checkpoint in {}: {e} (run a full tilegen first)",
            tmp_dir.display()
        ))
    })?;
    let stale = || {
        PipelineError(format!(
            "checkpoint in {} predates v{CHECKPOINT_VERSION} or is malformed; \
             re-run a full tilegen to regenerate it",
            tmp_dir.display()
        ))
    };
    let doc: serde_json::Value = serde_json::from_str(&content).map_err(|_| stale())?;
    if doc.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(CHECKPOINT_VERSION))
    {
        return Err(stale());
    }
    let coord = |i: usize| -> Result<f64, PipelineError> {
        doc["bounds"]
            .get(i)
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(stale)
    };
    let bounds = MercBbox {
        min_x: coord(0)?,
        min_y: coord(1)?,
        max_x: coord(2)?,
        max_y: coord(3)?,
    };
    let chunks = usize::try_from(
        doc.get("chunks")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(stale)?,
    )
    .map_err(|_| stale())?;
    let ocean_mode = match &doc["ocean"] {
        serde_json::Value::String(s) if s == "none" => OceanMode::None,
        serde_json::Value::String(s) if s == "computed" => OceanMode::Computed,
        serde_json::Value::Object(o) if o.contains_key("band") => OceanMode::Band {
            key: crate::ocean::OceanArtifactKey::from_json(&o["band"])?,
        },
        _ => return Err(PipelineError("invalid checkpoint ocean mode".to_string())),
    };
    // Interned back to the same 'static identifiers phase12 emits, so a
    // resumed value is indistinguishable from a freshly computed one.
    let path_name = |field: &str, allowed: [&'static str; 2]| {
        let found = doc["effective"].get(field).and_then(|v| v.as_str());
        allowed
            .into_iter()
            .find(|a| Some(*a) == found)
            .ok_or_else(|| {
                PipelineError(format!(
                    "invalid checkpoint effective.{field}: {}",
                    found.unwrap_or("(missing)")
                ))
            })
    };
    let provenance = CheckpointProvenance {
        input_xxh3_128: doc["input_xxh3_128"]
            .as_str()
            .ok_or_else(stale)?
            .to_string(),
        producer_config: doc.get("producer_config").cloned().ok_or_else(stale)?,
        effective: crate::provenance::Effective {
            coordinate_source: path_name("coordinate_source", ["inline", "node_store"])?,
            way_members: path_name("way_members", ["injected_v1", "relation_scan"])?,
            shared_node_pins: path_name("shared_node_pins", ["injected_v1", "block_local"])?,
        },
    };
    Ok((bounds, chunks, ocean_mode, provenance))
}

fn resolved_ocean_mode(config: &TilegenConfig) -> Result<OceanMode, PipelineError> {
    let Some(path) = config.ocean_tiles.as_deref() else {
        return Ok(if config.ocean_shapefile.is_some() {
            OceanMode::Computed
        } else {
            OceanMode::None
        });
    };
    if config.tile_format != TilePayloadFormat::Mvt
        || config.tile_compression != TileCompression::Gzip
        || config.min_zoom != 0
        || config.max_zoom != 14
    {
        eprintln!(
            "  Ocean artifact inactive: this tile format, compression, or zoom range uses computed ocean"
        );
        return Ok(if config.ocean_shapefile.is_some() {
            OceanMode::Computed
        } else {
            OceanMode::None
        });
    }
    if !path.exists() {
        return Err(PipelineError(format!(
            "configured ocean artifact does not exist: {}",
            path.display()
        )));
    }
    let declared = crate::ocean::OceanTiles::declared_key(path)?;
    if declared.compression_level != config.compression_level {
        eprintln!("  Ocean artifact inactive: compression level differs; using computed ocean");
        return Ok(if config.ocean_shapefile.is_some() {
            OceanMode::Computed
        } else {
            OceanMode::None
        });
    }
    let full = config.ocean_shapefile.as_deref().ok_or_else(|| {
        PipelineError("ocean artifact requires the full shapefile for key validation".to_string())
    })?;
    let simplified_shx = config
        .ocean_simplified_shapefile
        .as_ref()
        .map(|path| path.with_extension("shx"));
    let key = crate::ocean::OceanArtifactKey::from_inputs(
        full,
        &full.with_extension("shx"),
        config.ocean_simplified_shapefile.as_deref(),
        simplified_shx.as_deref(),
        0,
        14,
        config.compression_level,
    )?;
    Ok(OceanMode::Band { key })
}

// Note on extract consumption (adjudicated 2026-07-12): artifact-served
// interior tiles are NOT byte- or geometry-identical to the extract-computed
// ones - the pyramid's root-cell selection and row-band bisection depend on
// each piece's clipped extent, so a world-clipped piece descends with a
// different seam structure (denmark: 27 structural ocean diffs vs the
// computed baseline, displacements mostly 10-97 units). Both renderings pass
// every machine gate and the human viewer gate judged the artifact output
// equivalent, so extracts DO consume the artifact and the blessed baseline
// is artifact-active. Consequence: extract output depends on artifact
// presence - the standing regress gate assumes the gate machine carries the
// same data/ocean-tiles.pmtiles the blessed archive was built with.

fn ocean_pass_max_zooms(config: &TilegenConfig) -> Vec<u8> {
    let mut zooms = Vec::with_capacity(2);
    if config.ocean_simplified_shapefile.is_some() && config.min_zoom <= 7 {
        zooms.push(config.max_zoom.min(7));
    }
    if config.ocean_simplified_shapefile.is_none() || config.max_zoom >= 8 {
        zooms.push(config.max_zoom);
    }
    zooms
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
    let ocean_mode = resolved_ocean_mode(config)?;
    // Identifying the input means reading all of it, so where that read happens
    // matters. A resume must hash up front to validate the chunks, but then the
    // PBF is never read again. A full run defers it to just after phase12,
    // where the file is warm in page cache - hashing here instead would put a
    // cold serial pass in front of the reader and, at planet scale, cost a
    // second whole-file read plus the cache it evicts.
    let mut input_identity: Option<(String, u64)> = None;
    // Only a full run computes the effective paths; a resume inherits them
    // from the checkpoint written by the run that built the chunks.
    let mut resumed_effective = None;
    if skip.is_some() {
        let identity = crate::provenance::hash_file(&config.pbf_path)
            .map_err(|e| PipelineError(format!("could not read input PBF: {e}")))?;
        let (_, _, checkpoint_ocean_mode, checkpoint) = load_checkpoint(&config.tmp_dir)?;
        if checkpoint_ocean_mode != ocean_mode {
            return Err(PipelineError("checkpoint ocean mode differs from this run; --skip-to ocean, sort, and assemble cannot resume".to_string()));
        }
        if checkpoint.input_xxh3_128 != identity.0 {
            return Err(PipelineError(format!(
                "checkpoint was built from a different PBF (chunks: {}, this run: {}); \
                 --skip-to would mix two inputs into one archive - re-run a full tilegen",
                checkpoint.input_xxh3_128, identity.0,
            )));
        }
        // The chunks encode the producer config: zoom range, fanout caps,
        // simplification. They cannot be reinterpreted under different
        // settings, and reusing them would record settings the tiles were
        // never built under.
        let current_producer = crate::provenance::producer_config(config);
        if checkpoint.producer_config != current_producer {
            let diffs = crate::provenance::producer_config_diff(
                &checkpoint.producer_config,
                &current_producer,
            );
            return Err(PipelineError(format!(
                "checkpoint chunks were produced under a different config: {}; \
                 --skip-to cannot reinterpret them - re-run a full tilegen",
                diffs.join("; "),
            )));
        }
        resumed_effective = Some(checkpoint.effective);
        input_identity = Some(identity);
    }
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
    let mut active_ocean_artifact: Option<std::sync::Arc<crate::ocean::OceanTiles>> = None;

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
            // Deferred to here: phase12 has just streamed the whole PBF, so
            // this reads it back out of page cache rather than off the disk.
            let identity = crate::provenance::hash_file(&config.pbf_path)
                .map_err(|e| PipelineError(format!("could not read input PBF: {e}")))?;
            save_checkpoint(
                &config.tmp_dir,
                &bounds_out,
                sw.chunk_count(),
                &ocean_mode,
                &CheckpointProvenance {
                    input_xxh3_128: identity.0.clone(),
                    producer_config: crate::provenance::producer_config(config),
                    effective: phase12_stats
                        .as_ref()
                        .map(|s| s.effective)
                        .expect("phase12 stats are set immediately above"),
                },
            )?;
            input_identity = Some(identity);
            sw
        } else {
            // --skip-to ocean: load checkpoint, resume from PBF chunks
            let (_, pbf_chunks, checkpoint_ocean_mode, _) = load_checkpoint(&config.tmp_dir)?;
            if checkpoint_ocean_mode != ocean_mode {
                return Err(PipelineError(
                    "checkpoint ocean mode differs from this run; resume is unsafe".to_string(),
                ));
            }
            eprintln!("--- Skipping PBF phase ({pbf_chunks} chunks from checkpoint) ---");
            sort::SortWriter::resume(
                &config.tmp_dir.join(SORT_CHUNKS_DIR),
                sort_chunk_size,
                pbf_chunks,
                config.compress_sort_chunks,
            )?
        };

        // Load data_bounds (needed for ocean, always available from checkpoint or just computed)
        let (data_bounds, _, _, _) = load_checkpoint(&config.tmp_dir)?;

        active_ocean_artifact = if let (OceanMode::Band { key }, Some(path)) =
            (&ocean_mode, config.ocean_tiles.as_deref())
        {
            let artifact = std::sync::Arc::new(crate::ocean::OceanTiles::open(
                path,
                key,
                &data_bounds,
                &ocean_pass_max_zooms(config),
            )?);
            eprintln!(
                "  Ocean artifact active: {} runs, {} pass grids",
                artifact.runs_in(0, u64::MAX).len(),
                artifact.grids().len()
            );
            Some(artifact)
        } else {
            None
        };

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
                        active_ocean_artifact
                            .as_ref()
                            .and_then(|artifact| artifact.grids().first()),
                        config.no_ocean_simplify,
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
                        active_ocean_artifact
                            .as_ref()
                            .and_then(|artifact| artifact.grids().last()),
                        config.no_ocean_simplify,
                    )?;
                }
            } else {
                ocean_features = crate::ocean::process_ocean_shapefile(
                    ocean_path,
                    &data_bounds,
                    config.min_zoom,
                    config.max_zoom,
                    &mut sort_writer,
                    active_ocean_artifact
                        .as_ref()
                        .and_then(|artifact| artifact.grids().first()),
                    config.no_ocean_simplify,
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

    if active_ocean_artifact.is_none()
        && let (OceanMode::Band { key }, Some(path)) = (&ocean_mode, config.ocean_tiles.as_deref())
    {
        let (bounds, _, _, _) = load_checkpoint(&config.tmp_dir)?;
        let pass_max_zooms = ocean_pass_max_zooms(config);
        active_ocean_artifact = Some(std::sync::Arc::new(crate::ocean::OceanTiles::open(
            path,
            key,
            &bounds,
            &pass_max_zooms,
        )?));
    }

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
    ) = assemble::phase_assemble_with_ocean(
        &mut sort_reader,
        config,
        active_ocean_artifact,
        assemble::RunProvenance {
            input_identity,
            effective: phase12_stats
                .as_ref()
                .map(|s| s.effective)
                .or(resumed_effective),
            resumed_from: skip.map(|s| match s {
                SkipTo::Ocean => "ocean",
                SkipTo::Sort => "sort",
                SkipTo::Assemble => "assemble",
            }),
            ocean_artifact_key: match &ocean_mode {
                OceanMode::Band { key } => serde_json::from_str(&key.json()).ok(),
                OceanMode::None | OceanMode::Computed => None,
            },
            // The resolved mode, not a guess from which paths are set: ocean
            // runs only when the full shapefile is present, so a
            // simplified-only config produces no ocean at all.
            ocean_mode: match &ocean_mode {
                OceanMode::None => "none",
                OceanMode::Computed => "shapefile",
                OceanMode::Band { .. } => "artifact",
            },
        },
    )?;
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
        emit_counter_u64("way_pins_marked", s.way_pins_marked);
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

/// Build the durable world-ocean artifact without opening an OSM PBF. It uses
/// the normal ocean, external-sort, and partitioned assembly path so artifact
/// bytes obey the same encoding and compression rules as runtime ocean tiles.
pub fn ocean_build(
    full_shapefile: &Path,
    simplified_shapefile: Option<&Path>,
    output_path: &Path,
    tmp_dir: &Path,
    compression_level: u32,
    threads: usize,
) -> Result<(), PipelineError> {
    let simplified_shx = simplified_shapefile.map(|path| path.with_extension("shx"));
    let key = crate::ocean::OceanArtifactKey::from_inputs(
        full_shapefile,
        &full_shapefile.with_extension("shx"),
        simplified_shapefile,
        simplified_shx.as_deref(),
        0,
        14,
        compression_level,
    )?;
    drop(std::fs::remove_dir_all(tmp_dir));
    let chunks_dir = tmp_dir.join(SORT_CHUNKS_DIR);
    let mut writer = sort::SortWriter::new(
        &chunks_dir,
        DEFAULT_SORT_CHUNK_SIZE,
        sort::ChunkCompression::None,
    )?;
    let world = MercBbox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };
    if let Some(simplified) = simplified_shapefile {
        crate::ocean::process_ocean_shapefile(simplified, &world, 0, 7, &mut writer, None, false)?;
        crate::ocean::process_ocean_shapefile(
            full_shapefile,
            &world,
            8,
            14,
            &mut writer,
            None,
            false,
        )?;
    } else {
        crate::ocean::process_ocean_shapefile(
            full_shapefile,
            &world,
            0,
            14,
            &mut writer,
            None,
            false,
        )?;
    }
    writer.flush()?;
    let mut reader = writer.finish()?;
    let config = TilegenConfig {
        pbf_path: PathBuf::new(),
        output_path: output_path.to_path_buf(),
        tmp_dir: tmp_dir.to_path_buf(),
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        ocean_tiles: None,
        ocean_artifact_key: Some(key),
        ocean_only_metadata: true,
        no_ocean_simplify: false,
        skip_to: None,
        in_memory: false,
        compression_level,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads,
        way_inflight_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: DEFAULT_SORT_CHUNK_SIZE,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mvt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        seam_reconcile_layers: [0; shortbread::Layer::count()],
        fanout_caps: [0; shortbread::Layer::count()],
        polygon_simplify_factor: 1.0,
    };
    let _ = assemble::phase_assemble(&mut reader, &config)?;
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
    MembersForBlock, NodeStoreMode, PinSource, RawWay, annotate_block_shared_node_refs,
    build_way_plans, crosses_antimeridian, lon_e7_shifted_360, phase_read_and_process,
    select_node_store_mode,
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
