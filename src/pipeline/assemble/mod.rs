//! The assemble phase: the deferred per-partition sort merge, MVT/MLT
//! encoding, artifact ocean serving, and the ordered PMTiles write.
//!
//! - `schedule` owns the partition union, hot-partition pieces, the claim
//!   window, each partition's reader/encoder pair and the ordered writer -
//!   one lifecycle, kept whole because every writer exit must wake parked
//!   claimers.
//! - `encode` turns a batch of pending tiles into compressed payloads,
//!   including the artifact ocean layer for tiles the artifact owns.
//! - `artifact` plans the artifact's run copies between encoded tiles and
//!   extracts its ocean layer field. Admission, directory access and band
//!   ownership stay in `crate::ocean`.
//! - `report` tallies what the writer accepted and prints the per-zoom table.
//!
//! This file holds the shared tile and item types, archive setup and
//! provenance.

mod artifact;
mod encode;
mod report;
mod schedule;

use crate::archive::{self, BlobRef, LonLatBbox, MapCenter, PmtilesConfig, PmtilesWriter};
use crate::debug::{WAIT, emit_counter_u64, emit_counter_usize, wait_span};
use crate::shortbread::Layer;
use crate::sort;
use crate::tiling::TileId;
use pbfhogg::ElementReader;

use super::stats::TileSizeDiagnostics;
use super::{PipelineError, TilegenConfig, configured_tile_contract};

#[cfg(test)]
pub(super) use encode::encode_tile_batch;
// Used only by the MLT/MVT layer-model parity test.
#[cfg(all(test, feature = "mlt"))]
pub(super) use encode::{AssemblyScratch, prepare_non_empty_layers};

/// A tile's features collected from the sort reader, ready for parallel encoding.
pub(in crate::pipeline) struct PendingTile {
    pub(in crate::pipeline) tile_id: TileId,
    pub(in crate::pipeline) features: Vec<(Layer, Box<[u8]>)>,
}

const _: () = assert!(std::mem::size_of::<PendingTile>() == 32);

/// A payload encoded under the run's tile contract, ready for PMTiles.
/// Keep the contract on the run rather than every tile: a writer handles one
/// contract, and the 32-byte batch item size is asserted below. Repeating the
/// contract here would enlarge every queued tile without validating its bytes.
pub(in crate::pipeline) struct EncodedTile {
    pub(in crate::pipeline) tile_id: TileId,
    pub(in crate::pipeline) compressed: Vec<u8>,
}

const _: () = assert!(std::mem::size_of::<EncodedTile>() == 32);

/// Ordered work scheduled into the single PMTiles writer. `RunCopy` keeps a
/// shared artifact payload out of worker batches until the writer needs it.
enum PartitionItem {
    Encoded(EncodedTile),
    RunCopy {
        tile_id: TileId,
        run_length: u32,
        blob: BlobRef,
    },
}

pub(in crate::pipeline) const LAYER_COUNT: usize = Layer::count();

// Both payload encoders consume builders in Shortbread tile order, and the
// artifact's ocean layer field is appended after the encoded OSM layers.
// Keep the shared order checked.
const _: () = {
    assert!(Layer::Ocean.tile_order() as usize == LAYER_COUNT - 1);
    assert!(Layer::ALL[LAYER_COUNT - 1] as u8 == Layer::Ocean as u8);
};

/// What assemble hands back to the orchestrator: features read, tiles
/// addressed, unique payloads, the largest raw batch, writer dedup stats and
/// tile size diagnostics.
type AssembleTotals = (
    u64,
    u64,
    u64,
    usize,
    archive::DedupStats,
    TileSizeDiagnostics,
);

#[cfg(test)]
#[hotpath::measure]
pub(super) fn phase_assemble(
    sort_reader: &mut sort::SortReader,
    config: &TilegenConfig,
) -> Result<AssembleTotals, PipelineError> {
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
pub(in crate::pipeline) struct RunProvenance {
    /// XXH3-128 and byte length of the input PBF, hashed once by the
    /// orchestrator rather than recomputed here. The checkpoint stores the
    /// digest for resume validation. `None` only for the ocean artifact builder.
    pub(in crate::pipeline) input_identity: Option<crate::provenance::InputIdentity>,
    /// The paths phase12 actually took, or those the checkpoint recorded for
    /// a resumed run. `None` only when neither is available.
    pub(in crate::pipeline) effective: Option<crate::provenance::Effective>,
    /// The phase a resumed run started at; `None` for a full run.
    pub(in crate::pipeline) resumed_from: Option<super::SkipTo>,
    /// The resolved ocean producer, carrying its source identity.
    pub(in crate::pipeline) ocean_mode: super::OceanMode,
    /// Which file served the low zooms, from the run's ocean plan; `None`
    /// exactly when `ocean_mode` is `None`.
    pub(in crate::pipeline) low_zoom_source: Option<crate::provenance::ActiveLowZoomSource>,
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
pub(in crate::pipeline) fn phase_assemble_with_ocean(
    sort_reader: &mut sort::SortReader,
    config: &TilegenConfig,
    resources: &crate::resources::Resources,
    data_bounds: &crate::geometry::projection::MercBbox,
    ocean_tiles: Option<std::sync::Arc<crate::ocean::OceanTiles>>,
    run: RunProvenance,
) -> Result<AssembleTotals, PipelineError> {
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
    let schedule::AssembleCore {
        features_read,
        mut pmtiles,
        tally,
        max_batch_bytes,
        reader_ns,
        reader_total_ns,
        partition_workers,
        partition_count,
    } = schedule::phase_assemble_partitions(
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

    tally.print_per_zoom(config.min_zoom, config.max_zoom);

    Ok((
        features_read,
        tally.tiles_written,
        unique_tiles,
        max_batch_bytes,
        dedup_stats,
        tally.size_diag,
    ))
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
///
/// This stays on the pipeline side rather than in `provenance.rs` by
/// decision: it adapts resolved run state (`RunProvenance`, the ocean plan's
/// `contract_kind`) into the typed `Input` and `OceanContract` that
/// `provenance::build` takes. `provenance` is public and linked by brokkr, and
/// it already depends on `TilegenConfig` in the wrong direction; moving this
/// adapter in would add a dependency on pipeline run state as well.
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
