//! Pending tiles to compressed payloads: shared layer preparation, the MVT
//! and MLT encoders, per-zoom compression, and the artifact ocean layer for
//! tiles the artifact owns.

#[cfg(feature = "mlt")]
use crate::mlt;
use crate::mvt::{self, LayerBuilder};
use crate::shortbread::Layer;
use crate::tiling::MAX_ZOOM;
use crate::wire_format::add_feature_to_layer;

#[cfg(test)]
use super::super::TilePayloadFormat;
use super::super::{PipelineError, TileCompression, TileContract, compression_level_for_zoom};
use super::artifact::ArtifactPartitionCtx;
use super::{EncodedTile, LAYER_COUNT, PendingTile};

/// Brotli's I/O buffer size, independent of the tile coordinate grid.
const BROTLI_BUFFER_BYTES: usize = 4096;

/// Assembly state for exactly one tile, built per tile and dropped with it.
///
/// NOT pooled per thread, deliberately. The pooled version grew every buffer
/// to the fattest tile each rayon thread ever saw - geometry pools,
/// LayerBuilder feature and interning storage, merge scratch - summing to a
/// measured ~17 GB live plateau on NA locations assemble (identical under
/// mimalloc, glibc and jemalloc: live, not allocator retention). Rebuilding
/// per tile measured RSS 7.2 -> 1.8 GB on germany assemble-only with wall
/// unchanged (21.6s both ways). The scratches here are still reused across
/// the layers of one tile, which is the only reuse that ever paid.
pub(in crate::pipeline) struct AssemblyScratch {
    pub(in crate::pipeline) encode_scratch: mvt::EncodeScratch,
    pub(in crate::pipeline) merge_scratch: mvt::MergeScratch,
    pub(in crate::pipeline) line_merge_scratch: mvt::LineMergeScratch,
    pub(in crate::pipeline) mvt_buf: Vec<u8>,
    pub(in crate::pipeline) layers: [Option<LayerBuilder>; LAYER_COUNT],
}

impl AssemblyScratch {
    pub(in crate::pipeline) fn new() -> Self {
        AssemblyScratch {
            encode_scratch: mvt::EncodeScratch::new(),
            merge_scratch: mvt::MergeScratch::new(),
            line_merge_scratch: mvt::LineMergeScratch::new(),
            mvt_buf: Vec::new(),
            layers: [const { None }; LAYER_COUNT],
        }
    }
}

/// Encode + compress a batch of tiles with no ocean artifact.
#[cfg(test)]
pub(in crate::pipeline) fn encode_tile_batch(
    batch: &[PendingTile],
    compression_level: u32,
    tile_format: TilePayloadFormat,
    tile_compression: TileCompression,
) -> Result<Vec<EncodedTile>, PipelineError> {
    encode_partition_batch(
        batch,
        TileContract::from_fields(tile_format, tile_compression, compression_level),
        None,
    )
}

/// Encode + compress a batch of tiles in parallel using rayon. With an
/// artifact, every surviving tile the artifact owns takes its ocean layer
/// before compression.
#[hotpath::measure]
pub(super) fn encode_partition_batch(
    batch: &[PendingTile],
    contract: TileContract,
    artifact: Option<&ArtifactPartitionCtx<'_>>,
) -> Result<Vec<EncodedTile>, PipelineError> {
    match contract {
        TileContract::Mvt { compression, level } => {
            // The artifact stores MVT + gzip payloads and its run copies are
            // written verbatim beside these tiles, so any other contract
            // would mix payload encodings in one archive. Run admission
            // refuses that combination; this refuses it at the encoder too,
            // before any work and for an empty batch alike.
            if artifact.is_some() && compression != TileCompression::Gzip {
                return Err(artifact_contract_error());
            }
            encode_tile_batch_mvt(batch, level, compression, artifact)
        }
        TileContract::Mlt if artifact.is_some() => Err(artifact_contract_error()),
        #[cfg(feature = "mlt")]
        TileContract::Mlt => encode_tile_batch_mlt(batch),
        // Unreachable: run planning refuses MLT without the feature before
        // any work, so reaching this is a program fault. It still fails
        // rather than silently writing MVT under an MLT tile contract.
        #[cfg(not(feature = "mlt"))]
        TileContract::Mlt => Err(PipelineError::internal(
            "tile format mlt requires a build with the `mlt` cargo feature",
        )),
    }
}

fn artifact_contract_error() -> PipelineError {
    PipelineError::internal("the ocean artifact serves only MVT + gzip tiles")
}

/// Encode + compress a batch of MVT tiles in parallel using rayon.
#[hotpath::measure]
fn encode_tile_batch_mvt(
    batch: &[PendingTile],
    compression_level: u32,
    tile_compression: TileCompression,
    artifact: Option<&ArtifactPartitionCtx<'_>>,
) -> Result<Vec<EncodedTile>, PipelineError> {
    use rayon::prelude::*;

    let results: Vec<Result<Option<EncodedTile>, PipelineError>> = batch
        .par_iter()
        .map(|tile| encode_tile_mvt(tile, compression_level, tile_compression, artifact))
        .collect();

    let mut out = Vec::with_capacity(results.len());
    for result in results {
        if let Some(tile) = result? {
            out.push(tile);
        }
    }
    Ok(out)
}

/// One MVT tile. Survival is settled on the OSM content alone, before any
/// artifact bytes are read: a tile that encodes to nothing reserves no
/// address, and the artifact planner copies the artifact's payload there
/// untouched.
fn encode_tile_mvt(
    tile: &PendingTile,
    compression_level: u32,
    tile_compression: TileCompression,
    artifact: Option<&ArtifactPartitionCtx<'_>>,
) -> Result<Option<EncodedTile>, PipelineError> {
    let mut scratch = AssemblyScratch::new();
    let s = &mut scratch;

    let z = tile.tile_id.zoom();
    prepare_tile_layers(s, tile);
    // Borrow only the layer field so encoding can use its own buffers.
    let non_empty = non_empty_layers_in_tile_order(&s.layers);
    if non_empty.is_empty() {
        return Ok(None);
    }
    mvt::encode_tile_into(&mut s.mvt_buf, &non_empty, &mut s.encode_scratch);
    if s.mvt_buf.is_empty() {
        return Ok(None);
    }

    if let Some(artifact) = artifact
        && let Some(run) = artifact.splice_run(tile.tile_id)
    {
        // The producer prunes artifact-owned cells, so computed ocean here
        // means producer and consumer disagree on ownership, and appending
        // would draw the ocean twice. The encoder emits exactly the
        // non-empty builders, so this is the layer the tile would carry.
        if s.layers[Layer::Ocean as usize]
            .as_ref()
            .is_some_and(|layer| !layer.is_empty())
        {
            let (z, x, y) = tile.tile_id.coord();
            // The builders come from sort records read back from disk, so
            // this is not proof of a program fault.
            return Err(PipelineError::failed(format!(
                "non-band OSM tile {z}/{x}/{y} unexpectedly contains computed ocean"
            )));
        }
        // Ocean is last in tile order, so appending its field keeps the
        // tile's layers in Shortbread order.
        s.mvt_buf.extend_from_slice(&artifact.ocean_field(run)?);
    }

    // Per-zoom compression: boost low zooms, speed up high zooms.
    let level = compression_level_for_zoom(z, compression_level);
    let compressed = match tile_compression {
        TileCompression::Gzip => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
            std::io::Write::write_all(&mut encoder, &s.mvt_buf)?;
            encoder.finish()?
        }
        TileCompression::Brotli => {
            let mut compressed = Vec::new();
            let mut encoder =
                brotli::CompressorWriter::new(&mut compressed, BROTLI_BUFFER_BYTES, level, 22);
            std::io::Write::write_all(&mut encoder, &s.mvt_buf)?;
            drop(encoder);
            compressed
        }
    };
    Ok(Some(EncodedTile {
        tile_id: tile.tile_id,
        compressed,
    }))
}

/// Both encoders consume this stage so format selection cannot change the
/// geometry merge policy.
fn prepare_tile_layers(s: &mut AssemblyScratch, tile: &PendingTile) {
    for &(layer, ref data) in &tile.features {
        add_feature_to_layer(
            get_or_create_layer(&mut s.layers, usize::from(layer as u8)),
            data,
        );
    }

    for (li, layer) in s.layers.iter_mut().enumerate() {
        // Preserve the existing MVT ocean feature layout: it is also stored
        // in the durable world artifact. Removing this exclusion changes
        // those bytes and requires an OCEAN_POLICY_VERSION bump in ocean/policy.rs
        // and an artifact rebuild. This is a compatibility constraint, not
        // a topology requirement: the merger concatenates complete polygons,
        // and MapLibre's 500-ring cap applies per polygon, not per feature.
        if li == Layer::Ocean as usize {
            continue;
        }
        if let Some(lb) = layer.as_mut() {
            lb.merge_same_attr_geometries(&mut s.merge_scratch);
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
}

/// Build non-empty per-layer builders for the MLT encoder and model tests.
#[cfg(any(feature = "mlt", test))]
pub(in crate::pipeline) fn prepare_non_empty_layers<'a>(
    s: &'a mut AssemblyScratch,
    tile: &PendingTile,
) -> Vec<&'a LayerBuilder> {
    prepare_tile_layers(s, tile);
    non_empty_layers_in_tile_order(&s.layers)
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
fn encode_tile_batch_mlt(batch: &[PendingTile]) -> Result<Vec<EncodedTile>, PipelineError> {
    use rayon::prelude::*;

    let results: Vec<Result<Option<EncodedTile>, PipelineError>> = batch
        .par_iter()
        .map(|tile| {
            let mut scratch = AssemblyScratch::new();
            let non_empty = prepare_non_empty_layers(&mut scratch, tile);
            if non_empty.is_empty() {
                return Ok(None);
            }
            let encoded = mlt::encode_tile(&non_empty).map_err(|err| {
                let (z, x, y) = tile.tile_id.coord();
                PipelineError::failed(format!("mlt encode failed for tile {z}/{x}/{y}: {err}"))
            })?;
            Ok(Some(EncodedTile {
                tile_id: tile.tile_id,
                compressed: encoded,
            }))
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
    use crate::tiling::TileId;

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
                    Layer::Boundaries,
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
                    Layer::Ocean,
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
}
