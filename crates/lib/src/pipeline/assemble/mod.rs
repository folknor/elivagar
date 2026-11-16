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

use super::PipelineError;
#[cfg(test)]
use super::TilegenConfig;
use super::stats::TileSizeDiagnostics;

#[cfg(test)]
pub(super) use encode::encode_tile_batch;
// Used only by the MLT/MVT layer-model parity test.
#[cfg(all(test, feature = "mlt"))]
pub(super) use encode::{AssemblyScratch, prepare_non_empty_layers};

/// One raw batch of tiles read from a partition, stored flat: tile slots
/// index a feature table, features index one payload slab. The merge reader
/// appends each payload straight into the slab (`next_into`), so reading a
/// batch allocates nothing per record or per tile, and the batch's storage
/// is recycled to its reader once encoded. The per-record boxes and per-tile
/// Vecs this replaced were about 2.5 billion allocations at planet, each
/// made on a reader thread and freed on its encoder.
#[derive(Default)]
pub(in crate::pipeline) struct RawBatch {
    tiles: Vec<TileSlot>,
    features: Vec<FeatureRef>,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy)]
struct TileSlot {
    tile_id: TileId,
    /// Feature table range `start..end`.
    start: u32,
    end: u32,
}

#[derive(Clone, Copy)]
pub(in crate::pipeline) struct FeatureRef {
    layer: Layer,
    /// Slab range `start..start + len`.
    start: u32,
    len: u32,
}

fn batch_index(value: usize, what: &str) -> Result<u32, PipelineError> {
    u32::try_from(value).map_err(|_| {
        PipelineError::failed(format!(
            "assemble batch {what} exceeds the 4 GiB batch index range"
        ))
    })
}

impl RawBatch {
    pub(in crate::pipeline) fn len(&self) -> usize {
        self.tiles.len()
    }

    pub(in crate::pipeline) fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// Empty the batch, keeping its storage for the next one. Storage grown
    /// past `keep_bytes` by an outsized tile is released, so one fat tile
    /// does not pin its size in every recycled batch.
    pub(in crate::pipeline) fn recycle(&mut self, keep_bytes: usize) {
        self.tiles.clear();
        self.features.clear();
        self.bytes.clear();
        if self.bytes.capacity() > keep_bytes.saturating_mul(2) {
            self.bytes.shrink_to(keep_bytes);
        }
    }

    /// Open a new tile; the features pushed after it are its own.
    pub(in crate::pipeline) fn begin_tile(&mut self, tile_id: TileId) -> Result<(), PipelineError> {
        let at = batch_index(self.features.len(), "feature count")?;
        self.tiles.push(TileSlot {
            tile_id,
            start: at,
            end: at,
        });
        Ok(())
    }

    /// The slab the merge reader appends payloads to.
    pub(in crate::pipeline) fn bytes_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    /// Record a feature of the open tile whose payload already sits at
    /// `range` in the slab.
    pub(in crate::pipeline) fn push_feature_range(
        &mut self,
        layer: Layer,
        range: std::ops::Range<usize>,
    ) -> Result<(), PipelineError> {
        let start = batch_index(range.start, "payload slab")?;
        let len = batch_index(range.len(), "payload slab")?;
        batch_index(range.end, "payload slab")?;
        self.features.push(FeatureRef { layer, start, len });
        let end = batch_index(self.features.len(), "feature count")?;
        let tile = self
            .tiles
            .last_mut()
            .ok_or_else(|| PipelineError::internal("assemble feature pushed before any tile"))?;
        tile.end = end;
        Ok(())
    }

    /// Append a payload and record it as a feature of the open tile.
    #[cfg(test)]
    pub(in crate::pipeline) fn push_feature(
        &mut self,
        layer: Layer,
        data: &[u8],
    ) -> Result<(), PipelineError> {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(data);
        self.push_feature_range(layer, start..self.bytes.len())
    }

    /// The `i`th tile.
    pub(in crate::pipeline) fn tile(&self, i: usize) -> PendingTile<'_> {
        let slot = self.tiles[i];
        PendingTile {
            tile_id: slot.tile_id,
            features: &self.features[slot.start as usize..slot.end as usize],
            bytes: &self.bytes,
        }
    }
}

/// One tile of a [`RawBatch`], borrowed for encoding.
#[derive(Clone, Copy)]
pub(in crate::pipeline) struct PendingTile<'a> {
    pub(in crate::pipeline) tile_id: TileId,
    features: &'a [FeatureRef],
    bytes: &'a [u8],
}

impl<'a> PendingTile<'a> {
    /// The tile's features in sort order: layer and encoded payload.
    pub(in crate::pipeline) fn features(&self) -> impl Iterator<Item = (Layer, &'a [u8])> + 'a {
        let bytes = self.bytes;
        self.features.iter().map(move |f| {
            let start = f.start as usize;
            (f.layer, &bytes[start..start + f.len as usize])
        })
    }
}

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
    let provenance = TilesetProvenance {
        input_identity: crate::provenance::hash_file(&config.pbf_path)?,
        effective: None,
        resumed_from: None,
        ocean_mode: super::OceanMode::None,
        low_zoom_source: None,
    };
    let plan = super::RunPlan::resolve(config)?;
    let spec = AssembleSpec::tileset(
        &plan,
        crate::geometry::projection::MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        },
        None,
        provenance,
    )?;
    phase_assemble_with_ocean(
        sort_reader,
        spec,
        &crate::resources::Resources::resolve(config)?,
    )
}

/// Run facts that only the orchestrator knows, for archive provenance.
///
/// Passed in rather than re-derived here: `effective` is chosen in phase12
/// and is `None` when that phase did not run, and the artifact key belongs to
/// the resolved ocean mode. Re-deriving either in assemble would describe this
/// invocation rather than the run that produced the tiles.
#[derive(Debug)]
pub(in crate::pipeline) struct TilesetProvenance {
    /// XXH3-128 and byte length of the input PBF, hashed once by the
    /// orchestrator rather than recomputed here. The checkpoint stores the
    /// digest for resume validation.
    pub(in crate::pipeline) input_identity: crate::provenance::InputIdentity,
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

/// What kind of archive assembly writes. A tileset is an extract built from
/// an input PBF and carries run provenance; the world-ocean artifact has no
/// input PBF, is identified by its top-level `ocean_artifact` key alone and
/// carries no run provenance. The two never mix: an artifact build cannot
/// consume an artifact, and a tileset cannot claim an artifact key.
enum ArchiveRole<'a> {
    Tileset {
        /// The classified contract resolved before any phase work.
        contract: &'a crate::provenance::ContractConfig,
        pbf_path: &'a std::path::Path,
        pbf_name: String,
        data_bounds: crate::geometry::projection::MercBbox,
        ocean_tiles: Option<std::sync::Arc<crate::ocean::OceanTiles>>,
        provenance: TilesetProvenance,
    },
    OceanArtifact {
        key: crate::ocean::OceanArtifactKey,
    },
}

/// Everything assembly needs, built only by [`AssembleSpec::tileset`] and
/// [`AssembleSpec::ocean_artifact`], which hold the role's invariants.
pub(in crate::pipeline) struct AssembleSpec<'a> {
    tile: super::TileContract,
    zooms: crate::tiling::ZoomRange,
    chunk_compression: sort::ChunkCompression,
    output_path: &'a std::path::Path,
    tmp_dir: &'a std::path::Path,
    in_memory: bool,
    role: ArchiveRole<'a>,
}

impl<'a> AssembleSpec<'a> {
    /// An extract archive. Refuses a PBF path without a UTF-8 filename and
    /// ocean facts that disagree with each other: admitted artifact tiles
    /// exist exactly in artifact mode, under the same key the provenance
    /// records, and the low-zoom source is present exactly when there is
    /// ocean.
    pub(in crate::pipeline) fn tileset(
        config: &'a super::RunPlan,
        data_bounds: crate::geometry::projection::MercBbox,
        ocean_tiles: Option<std::sync::Arc<crate::ocean::OceanTiles>>,
        provenance: TilesetProvenance,
    ) -> Result<Self, PipelineError> {
        let pbf_name = config
            .pbf_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                PipelineError::internal("input PBF filename is unavailable for archive provenance")
            })?
            .to_string();
        match (&ocean_tiles, &provenance.ocean_mode) {
            (Some(tiles), super::OceanMode::Band { key }) if tiles.key() == key => {}
            (None, super::OceanMode::None | super::OceanMode::Computed { .. }) => {}
            _ => {
                return Err(PipelineError::internal(
                    "admitted ocean artifact tiles and the recorded ocean mode disagree",
                ));
            }
        }
        let ocean_kind = provenance
            .ocean_mode
            .contract_kind(provenance.low_zoom_source)?;
        if ocean_kind != config.contract.ocean.kind {
            return Err(PipelineError::internal(
                "assembly ocean facts and the resolved run contract disagree",
            ));
        }
        Ok(Self {
            tile: config.contract.tile,
            zooms: config.window,
            chunk_compression: config.compress_sort_chunks,
            output_path: &config.output_path,
            tmp_dir: &config.tmp_dir,
            in_memory: config.in_memory,
            role: ArchiveRole::Tileset {
                contract: &config.contract,
                pbf_path: &config.pbf_path,
                pbf_name,
                data_bounds,
                ocean_tiles,
                provenance,
            },
        })
    }

    /// The world-ocean artifact. Its whole tile contract follows from the
    /// key: MVT + gzip at the key's level over the artifact zoom range, from
    /// uncompressed chunks, streamed to disk. A key naming another level or
    /// zoom range would describe tiles this build does not write, so it is
    /// refused.
    pub(in crate::pipeline) fn ocean_artifact(
        key: crate::ocean::OceanArtifactKey,
        output_path: &'a std::path::Path,
        tmp_dir: &'a std::path::Path,
    ) -> Result<Self, PipelineError> {
        super::validate_compression_level(key.compression_level)?;
        let (first, last) = (
            crate::ocean::artifact::ARTIFACT_MIN_ZOOM,
            crate::ocean::artifact::ARTIFACT_MAX_ZOOM,
        );
        if (key.source.min_zoom, key.source.max_zoom) != (first, last) {
            return Err(PipelineError::internal(format!(
                "ocean artifact key names z{}-z{}, but the artifact is z{first}-z{last}",
                key.source.min_zoom, key.source.max_zoom
            )));
        }
        Ok(Self {
            tile: super::TileContract::Mvt {
                compression: super::TileCompression::Gzip,
                level: key.compression_level,
            },
            zooms: crate::tiling::ZoomRange::new(first, last)
                .expect("the artifact zoom contract is a profile range"),
            chunk_compression: sort::ChunkCompression::None,
            output_path,
            tmp_dir,
            in_memory: false,
            role: ArchiveRole::OceanArtifact { key },
        })
    }
}

/// Header bounds and center. An extract's bounds include the checkpoint's
/// ocean overlap buffer; Mercator y increases southward, so the southwest
/// and northeast corners swap y limits. The artifact declares the world.
fn archive_config(spec: &AssembleSpec<'_>) -> std::io::Result<PmtilesConfig> {
    use crate::geometry::projection::{Point, unproject};

    let bounds = match &spec.role {
        ArchiveRole::OceanArtifact { .. } => LonLatBbox {
            west: -180.0,
            south: -85.05,
            east: 180.0,
            north: 85.05,
        },
        ArchiveRole::Tileset { data_bounds, .. } => {
            let (west, south) = unproject(&Point::new(data_bounds.min_x, data_bounds.max_y));
            let (east, north) = unproject(&Point::new(data_bounds.max_x, data_bounds.min_y));
            LonLatBbox {
                west,
                south,
                east,
                north,
            }
        }
    };
    let (min_zoom, max_zoom) = (spec.zooms.first(), spec.zooms.last());
    let center = MapCenter {
        lon: (bounds.west + bounds.east) * 0.5,
        lat: (bounds.south + bounds.north) * 0.5,
        zoom: 2_u8.clamp(min_zoom, max_zoom),
    };
    PmtilesConfig::new(min_zoom, max_zoom, bounds, center)
}

#[allow(clippy::too_many_lines)]
// The spec is moved into the phase body's closures under the hotpath
// feature, where the measure macro's wrapping consumes it; without it clippy
// sees only the borrow.
#[allow(clippy::needless_pass_by_value)]
#[hotpath::measure]
pub(in crate::pipeline) fn phase_assemble_with_ocean(
    sort_reader: &mut sort::SortReader,
    spec: AssembleSpec<'_>,
    resources: &crate::resources::Resources,
) -> Result<AssembleTotals, PipelineError> {
    let pmtiles_config = archive_config(&spec)?;
    let mut pmtiles = if spec.in_memory {
        PmtilesWriter::new(pmtiles_config)
    } else {
        PmtilesWriter::new_streaming(pmtiles_config, spec.tmp_dir, spec.output_path)?
    };
    pmtiles.set_dedup_cap(resources.dedup_cap);
    let (tile_format, tile_compression) = spec.tile.writer();
    pmtiles.set_tile_contract(tile_format, tile_compression);
    let ocean_tiles = match &spec.role {
        ArchiveRole::OceanArtifact { key } => {
            pmtiles.set_ocean_artifact_metadata(key.clone())?;
            pmtiles.set_tileset(crate::pipeline::shortbread_tileset(&[
                crate::shortbread::Layer::Ocean,
            ]));
            None
        }
        ArchiveRole::Tileset { ocean_tiles, .. } => {
            pmtiles.set_tileset(crate::pipeline::shortbread_tileset(
                &crate::shortbread::Layer::ALL,
            ));
            ocean_tiles.as_deref()
        }
    };

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
        spec.tile,
        spec.chunk_compression,
        resources,
        ocean_tiles,
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
    match &spec.role {
        ArchiveRole::Tileset {
            contract,
            pbf_path,
            pbf_name,
            provenance,
            ..
        } => {
            pmtiles.set_source_pbf_filename(pbf_name.clone());
            let reader = ElementReader::from_path(pbf_path).map_err(|error| {
                PipelineError::failed("cannot read input PBF header for archive provenance")
                    .with_cause(error)
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
                (*contract).clone(),
                pbf_name.clone(),
                replication_timestamp,
                pbf_features,
                provenance,
            )?;
        }
        ArchiveRole::OceanArtifact { .. } => {
            // The artifact is identified by its top-level ocean_artifact
            // member and has no input PBF or run-shaped provenance to write.
            crate::debug::emit_counter("ocean_artifact_no_run_provenance", 1);
        }
    }
    let unique_tiles = pmtiles.unique_tile_count();
    let dedup_stats = pmtiles.dedup_stats().clone();
    {
        let _wait = wait_span(&WAIT.pmtiles_write);
        pmtiles.write_to(spec.output_path)?;
    }

    tally.print_per_zoom(spec.zooms.first(), spec.zooms.last());

    Ok((
        features_read,
        tally.tiles_written,
        unique_tiles,
        max_batch_bytes,
        dedup_stats,
        tally.size_diag,
    ))
}

/// Write the resolved comparability contract with the run's observed input
/// and effective execution paths. The contract is the one phase12 and the
/// checkpoint consumed; assembly never derives producer or encoding settings
/// from the caller's request. Artifact archives never reach this adapter.
fn write_provenance(
    pmtiles: &mut PmtilesWriter,
    config: crate::provenance::ContractConfig,
    name: String,
    replication_timestamp: Option<i64>,
    features: crate::provenance::PbfFeatures,
    run: &TilesetProvenance,
) -> Result<(), PipelineError> {
    let input = crate::provenance::Input {
        name,
        identity: run.input_identity,
        replication_timestamp,
        features,
    };
    let value = crate::provenance::build_resolved(&input, config, run.effective, run.resumed_from);
    pmtiles.set_elivagar_metadata(value)?;
    Ok(())
}
