// Tile generation pipeline orchestrator.
//
// Reads an OSM PBF file and produces a Shortbread-schema PMTiles v3 archive.
// Pipeline:
//   Phase 1+2: Single-pass PBF read - build node/way indices AND process features
//   Phase 3:   External merge sort by Hilbert tile ID
//   Phase 4:   Tile assembly (MVT encode + gzip) + PMTiles write

mod assemble;
mod boundary_prescan;
mod checkpoint;
pub(crate) mod emit;
mod ocean_build;
mod phase12;
mod relations;
mod stats;

use crate::archive;
#[cfg(test)]
use crate::geometry;
#[cfg(test)]
use crate::geometry::projection::MercBbox;
use crate::shortbread;
use crate::sort;
use crate::tiling::ZoomRange;

use std::path::PathBuf;

use crate::debug::{
    WAIT, emit_alloc_boundary, emit_counter, emit_counter_u64, emit_counter_usize, emit_marker,
    emit_wait_counters, wait_span,
};
use std::time::Instant;

pub use ocean_build::ocean_build;
use stats::Phase12Stats;
#[cfg(test)]
use stats::missing_ref_summary_lines;

mod error;
pub use error::{ArtifactRefusal, ErrorKind, KeyMismatch, PipelineError, ResumeRefusal};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Which pipeline phase to skip to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipTo {
    /// Skip PBF read, reuse PBF chunks, re-run ocean + sort + assemble.
    Ocean,
    /// Skip PBF + ocean, reuse all chunks, re-run sort + assemble.
    Sort,
    /// Skip PBF + ocean + sort setup, reuse existing chunks, run assemble only.
    Assemble,
}

/// Ocean producer selected for the checkpointed chunk set.
///
/// Every arm that produces chunks carries the identity of what produced them,
/// so a `--skip-to` resume can refuse chunks built from different inputs
/// instead of blending two contracts into one archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OceanMode {
    None,
    Computed { key: crate::ocean::OceanSourceKey },
    Band { key: crate::ocean::OceanArtifactKey },
}

impl OceanMode {
    fn contract_kind(
        &self,
        low_zoom_source: Option<crate::provenance::ActiveLowZoomSource>,
    ) -> Result<crate::provenance::OceanContractKind, PipelineError> {
        match (self, low_zoom_source) {
            (Self::None, None) => Ok(crate::provenance::OceanContractKind::None),
            (Self::Computed { key }, Some(low_zoom_source)) => {
                Ok(crate::provenance::OceanContractKind::Shapefile {
                    low_zoom_source,
                    source_key: key.to_json(),
                })
            }
            (Self::Band { key }, Some(low_zoom_source)) => {
                Ok(crate::provenance::OceanContractKind::Artifact {
                    low_zoom_source,
                    artifact_key: key.to_json(),
                })
            }
            _ => Err(PipelineError::internal(
                "ocean mode and low-zoom source disagree",
            )),
        }
    }
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
    /// First zoom of the archive, at most `max_zoom`.
    ///
    /// The zoom bounds are a truncation window over the profile range
    /// `0..=tiling::MAX_ZOOM`: geometry is constructed over each layer's own
    /// zoom range and only emission is restricted, so a narrowed archive holds
    /// the same tiles a full build holds at those zooms; computed ocean
    /// follows the same rule on its fixed z7 and z14 pass grids. The ocean
    /// artifact refuses any window but the full range.
    pub min_zoom: u8,
    /// Last zoom of the archive, at most `tiling::MAX_ZOOM`.
    pub max_zoom: u8,
    /// Explicit ocean sources, or no ocean when absent.
    pub ocean: Option<crate::OceanInputs>,
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
    /// Allow the flat node index when the PBF lacks `Sort.Type_then_ID` and
    /// `force_sorted` is false, regardless of input size, and bypass flat-index
    /// size guardrails. Unneeded with inline way coordinates. Expert debugging only.
    pub allow_unsafe_flat_index: bool,
    /// Worker sizing input, not a process-wide thread cap. The CLI sizes Rayon
    /// separately; library callers own their Rayon pool. Phase12 uses
    /// max(threads / 3, 1) decode workers and (threads / 4).clamp(2, 6) node
    /// workers in locations mode; way admission allows threads.max(8) blocks.
    /// Assembly derives partition workers from threads. Relation prepass, way
    /// dispatch and result drain also run on separate OS threads.
    /// There is no default here; the CLI's is
    /// [`crate::resources::default_threads`] (logical CPUs).
    pub threads: std::num::NonZeroUsize,
    /// Raw-byte budget for in-flight way processing, at least
    /// [`crate::resources::MIN_BUDGET`]. `None` = mode-aware default:
    /// - 128 MB (standard node-store path)
    /// - 4 GB (`locations_on_ways` mode)
    ///
    /// Safety net against individually huge blocks; the count ceiling
    /// (`threads.max(8)`) is the primary admission control.
    pub way_inflight_budget: Option<crate::resources::ByteBudget>,
    /// Byte budget for assemble tile batches, at least
    /// [`crate::resources::MIN_BUDGET`] (`None` = 32 MB).
    /// Controls memory during tile assembly. Dense urban tiles at z14 can
    /// make fixed-count batches very large.
    pub assemble_batch_budget: Option<crate::resources::ByteBudget>,
    /// Entry cap for the PMTiles payload dedup map (`None` = 1M).
    /// Tile content is identical under any cap; past it, duplicate payloads
    /// are stored instead of referenced, costing archive bytes. The map costs
    /// ~56 bytes/entry of RAM. Not part of the provenance comparability
    /// contract for the same reason budgets are not: it cannot change what a
    /// consumer decodes from any tile.
    pub dedup_cap: Option<std::num::NonZeroUsize>,
    /// Byte budget per sort chunk, at least
    /// [`crate::resources::MIN_SORT_BUDGET`] (`None` = 1 GB).
    /// Records buffer in memory up to this limit, then flush as a sorted
    /// chunk file to disk. Lower values reduce peak RSS during PBF processing
    /// at the cost of more chunk files in the merge phase.
    pub sort_chunk_size: Option<crate::resources::ByteBudget>,
    /// Use pre-resolved node coordinates from way elements instead of building
    /// a node store. Requires a PBF produced by `pbfhogg add-locations-to-ways`.
    /// Always detected from the PBF header's `LocationsOnWays` optional
    /// feature. When true, this field asserts that the header declares it.
    pub locations_on_ways: bool,
    /// Tile payload format (`mvt` default, `mlt` planned).
    pub tile_format: TilePayloadFormat,
    /// Tile compression algorithm for MVT payloads (`gzip` default, `brotli` optional).
    pub tile_compression: TileCompression,
    /// Compression algorithm for sort chunk files. Reduces disk I/O at the
    /// cost of CPU. Useful at planet scale where sort data exceeds available RAM.
    ///
    /// Naming `sort::ChunkCompression` here does not by itself make `sort`
    /// public: the module is documented library surface on its own account
    /// (the chunk writer and readers and their `ChunkStoreError`), so moving
    /// this type out would narrow nothing.
    pub compress_sort_chunks: sort::ChunkCompression,
    /// Per-layer fanout caps: maximum bbox tiles a polygon feature may touch
    /// at any zoom. When exceeded, the feature is skipped at that zoom.
    /// An absent cap is uncapped. Only supported polygon layers accept caps.
    pub fanout_caps: crate::FanoutCaps,
    /// Simplification tolerance multiplier for polygon layers. 1.0 = standard
    /// (same tolerance as lines). Values > 1.0 simplify more aggressively,
    /// reducing sort record volume. Polygon fill rendering is less sensitive
    /// to vertex precision than stroked lines, so higher tolerances are safe.
    /// Expected sweet spot: 1.0-2.0. Default: 1.0 (no change).
    pub polygon_simplify_factor: crate::SimplifyFactor,
}

const SORT_CHUNKS_DIR: &str = "sort_chunks";

/// The Shortbread tileset description handed to the archive writer: the
/// profile, not the container, owns the name, the `baselayer` type and which
/// layers an archive declares (every layer for an extract, the ocean layer
/// alone for the world ocean artifact).
pub(crate) fn shortbread_tileset(layers: &[shortbread::Layer]) -> archive::Tileset {
    archive::Tileset {
        name: "Shortbread".to_owned(),
        layer_type: "baselayer".to_owned(),
        vector_layers: layers
            .iter()
            .map(|layer| archive::VectorLayer {
                id: layer.name().to_owned(),
                min_zoom: layer.min_zoom(),
            })
            .collect(),
    }
}

/// Effective encoding, resolved from the public config without changing its API.
/// The gzip default is ignored for MLT; brotli is rejected by run before IO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TileContract {
    Mvt {
        compression: TileCompression,
        level: u32,
    },
    Mlt,
}

impl TileContract {
    pub(crate) fn from_fields(
        format: TilePayloadFormat,
        compression: TileCompression,
        level: u32,
    ) -> Self {
        match format {
            TilePayloadFormat::Mvt => Self::Mvt { compression, level },
            TilePayloadFormat::Mlt => Self::Mlt,
        }
    }

    fn writer(self) -> (archive::TileDataFormat, archive::TileDataCompression) {
        use crate::archive::{TileDataCompression, TileDataFormat};
        match self {
            Self::Mvt { compression, .. } => (
                TileDataFormat::Mvt,
                match compression {
                    TileCompression::Gzip => TileDataCompression::Gzip,
                    TileCompression::Brotli => TileDataCompression::Brotli,
                },
            ),
            Self::Mlt => (TileDataFormat::Mlt, TileDataCompression::None),
        }
    }

    #[cfg(test)]
    pub(crate) fn provenance_json(self) -> serde_json::Value {
        serde_json::to_value(self).expect("tile contract is JSON serializable")
    }

    fn compression_counter(self) -> i64 {
        match self {
            Self::Mvt {
                compression: TileCompression::Gzip,
                ..
            } => 0,
            Self::Mvt {
                compression: TileCompression::Brotli,
                ..
            } => 1,
            Self::Mlt => 2,
        }
    }
}

/// The provenance `config.tile` member. MLT records only format and
/// compression: it has no gzip level or zoom compression policy.
impl serde::Serialize for TileContract {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let (format, compression) = self.writer();
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("format", &format.metadata_payload())?;
        map.serialize_entry("compression", &compression.metadata_value())?;
        if let Self::Mvt { level, .. } = self {
            map.serialize_entry("base_compression_level", level)?;
            map.serialize_entry("compression_policy", COMPRESSION_POLICY)?;
        }
        map.end()
    }
}

/// Version of the base-level to per-zoom compression mapping.
/// This is deliberately excluded from OceanArtifactKey: changing compression
/// effort preserves decoded MVT bytes. Run copies can use different levels;
/// a tile that takes the artifact's ocean layer is compressed once, under
/// the current policy, with that layer already appended.
/// Rotating the world artifact and corpus for a size-only change buys no
/// content safety. Changes to decoded content belong to OCEAN_POLICY_VERSION.
pub(crate) const COMPRESSION_POLICY: &str = "zoom-v1";

fn validate_compression_level(level: u32) -> Result<(), PipelineError> {
    if level > 10 {
        return Err(PipelineError::config(format!(
            "compression level {level} exceeds maximum supported level 10"
        )));
    }
    Ok(())
}

pub(super) fn compression_level_for_zoom(z: u8, compression_level: u32) -> u32 {
    match z {
        0..=8 => compression_level.clamp(9, 10),
        13..=crate::tiling::MAX_ZOOM => compression_level.min(3),
        _ => compression_level,
    }
}

/// Resolve the ocean producer from the config, or fail.
///
/// Nothing here degrades silently. Every input is named explicitly on the
/// command line, so a named artifact that cannot be activated is a mistake in
/// the invocation, not a cue to quietly build something else: this used to
/// print "Ocean artifact inactive" to stderr and carry on computing, and
/// brokkr reads nothing from tilegen's stderr under `--bench`, so a bench run
/// could switch ocean paths on four different conditions and leave no trace in
/// the results row, the sidecar, or the archive.
fn resolve_ocean(
    config: &TilegenConfig,
    tile: TileContract,
    window: ZoomRange,
) -> Result<ResolvedOcean, PipelineError> {
    let Some(plan) = crate::ocean::OceanPlan::new(
        config.ocean.as_ref().map(|o| o.full.as_path()),
        config.ocean.as_ref().and_then(|o| o.simplified.as_deref()),
        config.ocean.as_ref().and_then(|o| o.artifact.as_deref()),
    )
    .map_err(PipelineError::config)?
    else {
        return Ok(ResolvedOcean::None);
    };
    let Some(path) = plan.artifact() else {
        let key = plan
            .source_key(window.first(), window.last())
            .map_err(|e| PipelineError::io("ocean shapefile is unreadable", e))?;
        return Ok(ResolvedOcean::Computed { plan, key });
    };
    let (tile_format, tile_compression) = tile.writer();
    let decoder = crate::archive::TileDecoder::for_contract(
        tile_format.header_tile_type(),
        tile_compression.header_compression(),
    );
    if decoder.is_err()
        || !crate::ocean::artifact::artifact_serves_window(window.first(), window.last())
    {
        // The request's tile contract and window, not the artifact's bytes,
        // rule this out, so it is a config refusal.
        return Err(PipelineError::config(format!(
            "ocean artifact {} was named but cannot serve this run: it is built for the MVT + gzip + z0-14 contract, and this run is {:?} + {:?} + z{}-z{}. Drop the artifact from --ocean to compute the ocean instead.",
            path.display(),
            config.tile_format,
            config.tile_compression,
            window.first(),
            window.last(),
        )));
    }
    let artifact = crate::ocean::OceanArtifact::admit(path, config.compression_level, || {
        plan.artifact_key(config.compression_level)
    })?;
    Ok(ResolvedOcean::Artifact { plan, artifact })
}

/// The ocean this run produces, resolved once at entry.
///
/// An artifact arm exists only with an admitted artifact, so no later step
/// re-pairs a key with a path. [`OceanMode`] is derived from this for the
/// checkpoint and provenance, which describe the run rather than hold it.
enum ResolvedOcean {
    None,
    Computed {
        plan: crate::ocean::OceanPlan,
        key: crate::ocean::OceanSourceKey,
    },
    Artifact {
        plan: crate::ocean::OceanPlan,
        artifact: crate::ocean::OceanArtifact,
    },
}

impl ResolvedOcean {
    fn mode(&self) -> OceanMode {
        match self {
            Self::None => OceanMode::None,
            Self::Computed { key, .. } => OceanMode::Computed { key: key.clone() },
            Self::Artifact { artifact, .. } => OceanMode::Band {
                key: artifact.key().clone(),
            },
        }
    }

    /// The identity of the ocean chunks this run produces, as the completion
    /// record stores it. The key is the one the passes are hashed under: the
    /// run window for computed ocean, the artifact's fixed z0-z14 for a band.
    fn chunks(&self) -> checkpoint::OceanChunks {
        match self {
            Self::None => checkpoint::OceanChunks::None,
            Self::Computed { key, .. } => checkpoint::OceanChunks::Computed {
                source_key: key.clone(),
            },
            Self::Artifact { artifact, .. } => checkpoint::OceanChunks::Band {
                source_key: artifact.key().source.clone(),
            },
        }
    }

    fn plan(&self) -> Option<&crate::ocean::OceanPlan> {
        match self {
            Self::None => None,
            Self::Computed { plan, .. } | Self::Artifact { plan, .. } => Some(plan),
        }
    }

    fn source_key(&self) -> Option<&crate::ocean::OceanSourceKey> {
        match self {
            Self::None => None,
            Self::Computed { key, .. } => Some(key),
            Self::Artifact { artifact, .. } => Some(&artifact.key().source),
        }
    }

    fn low_zoom_source(&self, min_zoom: u8) -> Option<crate::provenance::ActiveLowZoomSource> {
        self.plan().map(|plan| plan.low_zoom_source(min_zoom))
    }
}

// Note on extract consumption (adjudicated 2026-07-12): artifact-served
// interior tiles are NOT byte- or geometry-identical to the extract-computed
// ones - the pyramid's root-cell selection and recursive buffered-range
// bisection depend on each piece's clipped extent, so a world-clipped piece descends with a
// different seam structure (denmark: 27 structural ocean diffs vs the
// computed baseline, displacements mostly 10-97 units). Both renderings pass
// every machine gate and the human viewer gate judged the artifact output
// equivalent, so extracts DO consume the artifact and the committed corpus
// baseline (corpus/<dataset>/) is artifact-active: its contract records the
// artifact key, so a gate run must carry the same data/ocean-tiles.pmtiles the
// baseline was captured from, and rotating the artifact forces a corpus
// rotation (corpus check refuses on the key until then).

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

fn emit_allocator_boundary(name: &str) {
    emit_alloc_boundary(name);
}

use crate::debug::layer_stats_enabled;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The run's zoom window, the one place the config's two numbers become a
/// validated range. An excessive `max_zoom` is reported before a reversed pair.
fn zoom_window(min_zoom: u8, max_zoom: u8) -> Result<ZoomRange, PipelineError> {
    if max_zoom > crate::tiling::MAX_ZOOM {
        return Err(PipelineError::config(format!(
            "max_zoom {} exceeds maximum supported zoom level {}",
            max_zoom,
            crate::tiling::MAX_ZOOM
        )));
    }
    ZoomRange::new(min_zoom, max_zoom).ok_or_else(|| {
        PipelineError::config(format!(
            "min_zoom {min_zoom} is greater than max_zoom {max_zoom}"
        ))
    })
}

/// Validated request parts shared by execution, checkpoints and provenance.
/// No phase accepts the caller DTO after this boundary.
pub(crate) struct RunPlan {
    window: ZoomRange,
    contract: crate::provenance::ContractConfig,
    ocean: ResolvedOcean,
    resources: crate::resources::Resources,
    pbf_path: PathBuf,
    output_path: PathBuf,
    tmp_dir: PathBuf,
    skip_to: Option<SkipTo>,
    in_memory: bool,
    compress_sort_chunks: sort::ChunkCompression,
    force_sorted: bool,
    allow_unsafe_flat_index: bool,
    locations_on_ways: bool,
}

impl RunPlan {
    fn resolve(config: &TilegenConfig) -> Result<Self, PipelineError> {
        if config
            .pbf_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_none()
        {
            return Err(PipelineError::config(
                "input PBF path must have a UTF-8 filename so archive provenance can identify it",
            ));
        }
        if config.tile_format == TilePayloadFormat::Mlt
            && config.tile_compression == TileCompression::Brotli
        {
            return Err(PipelineError::config(
                "MLT payloads are uncompressed; brotli is MVT only",
            ));
        }
        #[cfg(not(feature = "mlt"))]
        if config.tile_format == TilePayloadFormat::Mlt {
            return Err(PipelineError::config(
                "--tile-format mlt requires a build with the `mlt` feature",
            ));
        }
        if config.tile_format == TilePayloadFormat::Mvt {
            validate_compression_level(config.compression_level)?;
        }
        let window = zoom_window(config.min_zoom, config.max_zoom)?;

        let resources = crate::resources::Resources::resolve(config)?;
        let contract = crate::provenance::ContractConfig::new(
            config,
            crate::provenance::OceanContract {
                kind: crate::provenance::OceanContractKind::None,
                runtime_simplification: true,
            },
        );
        let ocean = resolve_ocean(config, contract.tile, window)?;
        let contract = contract.with_ocean(crate::provenance::OceanContract {
            kind: ocean
                .mode()
                .contract_kind(ocean.low_zoom_source(window.first()))?,
            // Always true since the runtime bypass was removed; retained in
            // the serialized comparability contract for older archives.
            runtime_simplification: true,
        });
        Ok(Self {
            window,
            contract,
            ocean,
            resources,
            pbf_path: config.pbf_path.clone(),
            output_path: config.output_path.clone(),
            tmp_dir: config.tmp_dir.clone(),
            skip_to: config.skip_to,
            in_memory: config.in_memory,
            compress_sort_chunks: config.compress_sort_chunks,
            force_sorted: config.force_sorted,
            allow_unsafe_flat_index: config.allow_unsafe_flat_index,
            locations_on_ways: config.locations_on_ways,
        })
    }
}

/// # Errors
///
/// Returns [`PipelineError`], whose [`ErrorKind`] says whether the request
/// was invalid, the input or resume state or ocean artifact was refused, the
/// OS reported an I/O failure, or the failure is unclassified.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
#[hotpath::measure]
pub fn run(config: &TilegenConfig) -> Result<(), PipelineError> {
    run_inner(config).map_err(|error| error.chunk_context(config.skip_to.is_some()))
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn run_inner(config: &TilegenConfig) -> Result<(), PipelineError> {
    crate::debug::reset_run_counters();
    crate::ocean::reset_ocean_counters();
    sort::reset_sort_counters();
    let mut config = RunPlan::resolve(config)?;
    let window = config.window;
    let resources = &config.resources;
    resources.emit();
    let total_start = Instant::now();
    let skip = config.skip_to;
    let ocean = &config.ocean;
    let ocean_mode = ocean.mode();
    let low_zoom_source = ocean.low_zoom_source(window.first());
    // Identifying the input means reading all of it, so where that read happens
    // matters. A resume must hash up front to validate the chunks before any
    // work is done under them. A full run hashes on a background thread
    // overlapped with phase12, which reads the same file far slower than the
    // hash does - hashing serially after phase12 was measured at ~15s on the
    // 19 GB north-america input, because by then the file's head is evicted
    // from page cache and the "read it back out of cache" assumption fails at
    // exactly the scale where the read costs most.
    let mut input_identity: Option<crate::provenance::InputIdentity> = None;
    // A resume inherits the extent, chunk counts and effective paths from the
    // records written by the run that built the chunks; see `checkpoint`.
    let mut resume: Option<checkpoint::Resume> = None;
    if let Some(skip) = skip {
        // Phase12 checks this on full runs; a resume must honor the same
        // explicit header assertion before reusing any chunks.
        if config.locations_on_ways {
            let reader = pbfhogg::ElementReader::from_path(&config.pbf_path)
                .map_err(|error| PipelineError::failed("failed to open PBF").with_cause(error))?;
            phase12::resolve_locations_on_ways(true, reader.header().has_locations_on_ways())?;
        }
        let identity = crate::provenance::hash_file(&config.pbf_path)
            .map_err(|e| PipelineError::io("could not read input PBF", e))?;
        resume = Some(checkpoint::admit(
            &config.tmp_dir,
            skip,
            &checkpoint::RunIdentity {
                input_xxh3_128: identity.xxh3_128,
                producer_config: config.contract.producer.json(),
                sort_chunk_compression: config.compress_sort_chunks,
                ocean: ocean.chunks(),
            },
        )?);
        input_identity = Some(identity);
    }
    let resumed_sort_chunks = resume
        .as_ref()
        .and_then(|r| r.completion.as_ref())
        .map(|c| c.chunks);
    let resumed_effective = resume.as_ref().map(|r| r.pbf.effective);
    // The data extent: phase12's on a full run, the checkpoint's on a resume.
    let mut data_bounds = resume.as_ref().map(|r| r.pbf.bounds());
    emit_allocator_boundary("run_start");

    eprintln!(
        "=== Tilegen: {} → {}",
        config.pbf_path.display(),
        config.output_path.display()
    );
    eprintln!("    Zoom range: z{}-z{}", window.first(), window.last());
    eprintln!("    Tmp dir:    {}", config.tmp_dir.display());
    eprintln!("    Tile contract: {:?}", config.contract.tile);
    if (config.contract.producer.simplify_factor() - 1.0).abs() > f64::EPSILON {
        eprintln!(
            "    Polygon simplify factor: {:.2}",
            config.contract.producer.simplify_factor()
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
    let mut ocean_band: Option<crate::ocean::OceanBand> = None;

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
            // Full run: clean tmp dir and run PBF phase. The cleanup must
            // succeed: a partial one could leave an earlier run's records
            // beside the chunks this run writes, and a later resume would
            // read them as describing those chunks.
            match std::fs::remove_dir_all(&config.tmp_dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(PipelineError::io(
                        format!(
                            "could not clear tmp dir {} before a full run",
                            config.tmp_dir.display()
                        ),
                        error,
                    ));
                }
            }
            std::fs::create_dir_all(&config.tmp_dir)?; // io::Error message is sufficient context.

            let input_hash = crate::provenance::BackgroundHash::spawn(config.pbf_path.clone());
            let phase12_start = Instant::now();
            let (mut sw, bounds_out, p12_stats) =
                phase12::phase_read_and_process(&config, resources)?;
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
            // The hash has been running since before phase12 started and
            // finishes long before it; any wait recorded here means the
            // overlap assumption broke and the counter names it.
            let identity = {
                let _wait = wait_span(&WAIT.input_hash_join);
                input_hash
                    .join()
                    .map_err(|e| PipelineError::io("could not read input PBF", e))?
            };
            checkpoint::PbfCheckpoint::new(
                &bounds_out,
                sw.chunk_count(),
                identity.xxh3_128,
                config.contract.producer.json(),
                config.compress_sort_chunks,
                phase12_stats
                    .as_ref()
                    .map(|s| s.effective)
                    .expect("phase12 stats are set immediately above"),
            )
            .save(&config.tmp_dir)?;
            data_bounds = Some(bounds_out);
            input_identity = Some(identity);
            sw
        } else {
            // --skip-to ocean: keep the PBF chunks, rebuild the ocean. The
            // ocean input may differ from the one that built the old ocean
            // chunks; those are deleted, and the completion record is
            // cleared first so a failed pass cannot look complete.
            let pbf_chunks = resume
                .as_ref()
                .map(|r| r.pbf.chunks)
                .expect("a resume was admitted above");
            eprintln!("--- Skipping PBF phase ({pbf_chunks} chunks from checkpoint) ---");
            checkpoint::ChunkCompletion::clear(&config.tmp_dir)?;
            sort::SortWriter::resume(
                &config.tmp_dir.join(SORT_CHUNKS_DIR),
                resources.sort_chunk,
                pbf_chunks,
                config.compress_sort_chunks,
            )?
        };

        let data_bounds = data_bounds.expect("set by phase12 or the admitted resume");
        let band = crate::ocean::OceanBand::for_bounds(&data_bounds);
        ocean_band = Some(band);
        // Only an artifact-served run leaves tiles to someone else; a
        // computed run owns every tile it emits.
        let producer_band = matches!(ocean, ResolvedOcean::Artifact { .. }).then_some(&band);

        // --- Ocean shapefile processing ---
        // The plan fixes the passes: z0-z7 from the simplified file when one
        // is named and from the full file otherwise, z8-z14 from the full
        // file. The window only decides what each pass emits.
        emit_marker("OCEAN_START");
        emit_allocator_boundary("ocean_start");
        ocean_elapsed = if let (Some(plan), Some(source_key)) = (ocean.plan(), ocean.source_key()) {
            let ocean_start = Instant::now();
            eprintln!("--- Ocean shapefile ---");
            let mut ocean_features: u64 = 0;
            for pass in crate::ocean::OceanPass::ALL {
                let Some(window) = pass.emission(window) else {
                    continue;
                };
                let source = plan.source(pass);
                eprintln!(
                    "  {pass:?} pass (z{}-z{}) from {}:",
                    window.zooms().first(),
                    window.zooms().last(),
                    source.shp().display()
                );
                ocean_features += crate::ocean::process_ocean_shapefile_checked(
                    source.shp(),
                    &data_bounds,
                    window,
                    &mut sort_writer,
                    producer_band,
                    plan.pass_hashes(source_key, pass)
                        .map_err(PipelineError::internal)?,
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
    // then save the completion count for later resumes.
    if let Some(ref mut sw) = sort_writer {
        let _wait = wait_span(&WAIT.sort_flush);
        sw.flush()?;
    }
    let sort_chunks = sort_writer.as_ref().map(sort::SortWriter::chunk_count);
    if let Some(count) = sort_chunks {
        checkpoint::ChunkCompletion::new(count, ocean.chunks()).save(&config.tmp_dir)?;
    }
    let expected_sort_chunks = sort_chunks.or(resumed_sort_chunks);
    let (mut sort_reader, phase3_elapsed, sort_rss) = if skip == Some(SkipTo::Assemble) {
        let sr = {
            let _wait = wait_span(&WAIT.sort_open);
            sort::SortReader::from_dir(
                &config.tmp_dir.join(SORT_CHUNKS_DIR),
                expected_sort_chunks,
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
                expected_sort_chunks,
                config.compress_sort_chunks,
            )?
        };
        (sr, Some(phase3_start.elapsed()), peak_rss_kb())
    };

    emit_marker("SORT_END");
    emit_allocator_boundary("sort_end");

    let data_bounds = data_bounds.expect("set by phase12 or the admitted resume");
    // The band the ocean phase produced under, or on a resume past it the
    // same function of the same checkpoint bounds.
    let ocean_band =
        ocean_band.unwrap_or_else(|| crate::ocean::OceanBand::for_bounds(&data_bounds));
    let active_ocean_artifact = match std::mem::replace(&mut config.ocean, ResolvedOcean::None) {
        ResolvedOcean::Artifact { artifact, .. } => {
            let tiles = artifact.into_tiles(ocean_band)?;
            eprintln!("  Ocean artifact active: {} runs", tiles.run_count());
            Some(std::sync::Arc::new(tiles))
        }
        ResolvedOcean::None | ResolvedOcean::Computed { .. } => None,
    };

    let input_identity = input_identity.ok_or_else(|| {
        PipelineError::internal(
            "input PBF identity is unavailable; refusing to write an archive without provenance",
        )
    })?;

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
        assemble::AssembleSpec::tileset(
            &config,
            data_bounds,
            active_ocean_artifact,
            assemble::TilesetProvenance {
                input_identity,
                effective: phase12_stats
                    .as_ref()
                    .map(|s| s.effective)
                    .or(resumed_effective),
                resumed_from: skip,
                ocean_mode: ocean_mode.clone(),
                low_zoom_source,
            },
        )?,
        resources,
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
        match config.contract.tile {
            TileContract::Mvt { .. } => 0,
            TileContract::Mlt => 1,
        },
    );
    emit_counter(
        "tile_compression",
        config.contract.tile.compression_counter(),
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
            if recs > 0
                && let Some(&layer) = shortbread::Layer::ALL.get(i)
            {
                let name = layer.name();
                emit_counter_u64(&format!("sort_layer_{name}_records"), recs);
                emit_counter_u64(&format!("sort_layer_{name}_bytes"), bytes);
                if !layer_stats {
                    continue;
                }
                for z in 0..=crate::tiling::MAX_ZOOM {
                    let idx = crate::sort::LayerZoom::new(layer, z).expect("profile zoom");
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
                            u64::from(s.fanout_stats.percentile(idx, 0.50)),
                        );
                        emit_counter_u64(
                            &format!("sort_layer_{name}_z{z}_fanout_p95"),
                            u64::from(s.fanout_stats.percentile(idx, 0.95)),
                        );
                        emit_counter_u64(
                            &format!("sort_layer_{name}_z{z}_fanout_p99"),
                            u64::from(s.fanout_stats.percentile(idx, 0.99)),
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
                    for z in 0..=crate::tiling::MAX_ZOOM {
                        let idx = crate::sort::LayerZoom::new(layer, z).expect("profile zoom");
                        let n = s.fanout_stats.features_above(idx, thresh);
                        if n > 0 {
                            emit_counter_u64(&format!("sort_layer_{name}_z{z}_above_{thresh}"), n);
                        }
                    }
                }
            }
        }
        for (rank, event) in s.fanout_stats.top_capped.iter().enumerate() {
            let prefix = format!("fanout_capped_top_{}", rank + 1);
            emit_counter_u64(&format!("{prefix}_osm_id"), event.osm_id);
            emit_counter_u64(
                &format!("{prefix}_layer"),
                u64::from(event.at.layer() as u8),
            );
            emit_counter_u64(&format!("{prefix}_zoom"), u64::from(event.at.zoom()));
            emit_counter_u64(&format!("{prefix}_bbox_tiles"), event.bbox_tiles);
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
        let (z, x, y) = tile_size_diag.max_tile.tile_id.coord();
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
        let (z, x, y) = t.tile_id.coord();
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
use crate::geometry::projection::Point;
#[cfg(test)]
use crate::geometry::tiles::merc_bbox;
#[cfg(all(test, feature = "mlt"))]
use crate::mlt;
#[cfg(test)]
use crate::multipolygon::{MemberWay, WayRole};
#[cfg(test)]
use crate::sort::SortRecord;
#[cfg(test)]
use assemble::{PendingTile, encode_tile_batch, phase_assemble};
// Used only by the MLT/MVT layer-model parity test.
#[cfg(all(test, feature = "mlt"))]
use assemble::{AssemblyScratch, prepare_non_empty_layers};
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
    MissingRefStats, MissingRefStatsAtomic, OversizeTile, TILE_OVERSIZE_SEVERE_BYTES,
    TILE_OVERSIZE_TOP_N, TILE_OVERSIZE_WARN_BYTES, TileSizeDiagnostics, insert_top_oversized,
    record_tile_size_diagnostics,
};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../pipeline_tests.rs"]
mod tests;
