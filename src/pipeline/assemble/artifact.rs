//! Consumer half of the durable ocean artifact: which addresses take a run
//! copy, which encoded tiles take the artifact's ocean layer, and the
//! extraction of that layer field. Ownership itself is `OceanBand`'s.

use crate::archive::{BlobRef, TileRun};
use crate::ocean::band::TileRect;
use crate::shortbread::Layer;
use crate::tiling::{MAX_ZOOM, TileId, ZOOM_COUNT, zoom_ids};

use super::super::PipelineError;
use super::{EncodedTile, PartitionItem};

pub(super) struct ArtifactPartitionCtx<'a> {
    pub(super) ocean: &'a crate::ocean::OceanTiles,
    band_empty: bool,
    copy_ranges_by_zoom: Vec<Vec<(u64, u64)>>,
}

impl<'a> ArtifactPartitionCtx<'a> {
    pub(super) fn new(ocean: &'a crate::ocean::OceanTiles) -> Result<Self, PipelineError> {
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
    pub(super) fn splice_run(&self, tile_id: TileId) -> Option<TileRun> {
        let (z, x, y) = tile_id.coord();
        if self.ocean.band().computed_owns(z, x, y) {
            return None;
        }
        self.ocean.run_covering(tile_id)
    }

    /// The raw MVT layer field (tag, length and body) of `run`'s payload,
    /// ready to append to an uncompressed OSM tile.
    ///
    /// Tiles sharing one run gunzip and parse its blob again each time, on
    /// purpose. Measured on denmark locations, artifact-active, 2026-11-14:
    /// 30895 spliced tiles over 10647 distinct blobs cost 111 ms summed
    /// worker time (probe overhead included) in a 7.4 s run, so a perfect
    /// cache saves a few ms of wall. A cache must also extract lazily,
    /// after survival is settled, and could not live on this context: it
    /// is built once and shared across every partition, so a cache here
    /// would retain decompressed fields for the whole assembly. Reopen
    /// only if a larger run shows repeated extraction of expensive blobs
    /// costing real CPU or critical-path time.
    pub(super) fn ocean_field(&self, run: TileRun) -> Result<Vec<u8>, PipelineError> {
        ocean_layer_field(&self.ocean.raw_blob(run.blob())?)
    }
}

pub(super) fn artifact_run_copy_items(
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
                append_artifact_run_copy(&mut items, cursor, sub_end, blob)?;
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
                    blob,
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
            PipelineError::artifact(format!(
                "ocean artifact tile id {raw} lies beyond z{MAX_ZOOM}"
            ))
        })
}

/// Queue a copy of `blob` over the ids `[start, end)`. The bounds are raw
/// like every id-range boundary here (see `crate::tiling`): range arithmetic
/// steps to exclusive ends that need not be addresses, and the start becomes
/// a `TileId` once it names the run's first tile.
fn append_artifact_run_copy(
    items: &mut Vec<PartitionItem>,
    start: u64,
    end: u64,
    blob: BlobRef,
) -> Result<(), PipelineError> {
    if start >= end {
        return Ok(());
    }
    items.push(PartitionItem::RunCopy {
        tile_id: artifact_tile(start)?,
        run_length: u32::try_from(end - start)
            .map_err(|_| PipelineError::internal("artifact split run length exceeds u32"))?,
        blob,
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

/// The artifact payload's single ocean layer as a raw MVT tile field, taken
/// verbatim: never decoded and re-encoded, so spliced bytes are the
/// artifact's bytes.
///
/// A strict envelope parse admits a tile message holding nothing but layer
/// fields, so once it finds exactly one layer, named ocean, the whole
/// decompressed payload is that one field's tag, length and body - there is
/// no offset to find. This admits the envelope only; features and values
/// are the artifact build's to have encoded, and are not re-validated per
/// spliced tile.
fn ocean_layer_field(compressed: &[u8]) -> Result<Vec<u8>, PipelineError> {
    // The payload is already in memory (the artifact is mapped), so a
    // decompression or parse failure rejects the artifact's own bytes.
    let raw = crate::archive::gzip_decompress(compressed).map_err(|error| {
        PipelineError::artifact("artifact tile is not valid gzip").with_cause(error)
    })?;
    let layers = crate::mvt::message::parse_tile(&raw, crate::tile_detail::Strictness::Strict)
        .map_err(|error| PipelineError::artifact(format!("artifact tile: {error}")))?;
    if layers.len() != 1 || layers[0].name != Layer::Ocean.name() {
        return Err(PipelineError::artifact(
            "artifact tile must contain exactly one ocean layer",
        ));
    }
    drop(layers);
    Ok(raw)
}

/// Where artifact gap filling stands within one partition or piece: every
/// address below `next` has been planned, and `end` is the exclusive end of
/// the range this reader owns.
pub(super) struct ArtifactCursor {
    pub(super) next: u64,
    pub(super) end: u64,
}

/// Interleave a batch's surviving encoded tiles with the artifact run copies
/// that fill the addresses between them. Encoded tiles the artifact owns
/// already carry its ocean layer: the encoder appended it before compressing.
///
/// Only encoded tiles reserve an address. A pending tile whose layers all
/// came out empty encodes to nothing, and its address falls inside a copy
/// range like any other gap, so a tile the artifact owns keeps its ocean
/// whether or not OSM content survived there. On the last batch the tail up to
/// the range end is copied and the cursor closes.
pub(super) fn plan_artifact_items(
    artifact: &ArtifactPartitionCtx<'_>,
    cursor: &mut ArtifactCursor,
    encoded: Vec<EncodedTile>,
    is_last: bool,
) -> Result<Vec<PartitionItem>, PipelineError> {
    let mut items = Vec::with_capacity(encoded.len());
    for tile in encoded {
        let id = tile.tile_id.raw();
        // Encoded ids come from this reader's own sorted input, so they are
        // strictly increasing and inside its range; anything else would make
        // the copies below overlap or reorder the writer's stream.
        if id < cursor.next || id >= cursor.end {
            return Err(PipelineError::internal(format!(
                "encoded tile {id} lies outside the unplanned partition range {}..{}",
                cursor.next, cursor.end
            )));
        }
        items.extend(artifact_run_copy_items(cursor.next, id, artifact)?);
        items.push(PartitionItem::Encoded(tile));
        cursor.next = id + 1;
    }
    if is_last {
        items.extend(artifact_run_copy_items(cursor.next, cursor.end, artifact)?);
        cursor.next = cursor.end;
    }
    Ok(items)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::super::{TileCompression, TileContract};
    use super::super::PendingTile;
    use super::super::encode::encode_partition_batch;
    use super::super::schedule::{PartitionBatchMeta, encode_and_send_partition_batch};
    use super::*;
    use crate::archive::{self, LonLatBbox, MapCenter};
    use std::io::Write;

    const MVT_GZIP_6: TileContract = TileContract::Mvt {
        compression: TileCompression::Gzip,
        level: 6,
    };

    fn tile(raw: u64) -> TileId {
        TileId::new(raw).unwrap()
    }

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

    /// The encoder takes the splice decision from the same `splice_run` the
    /// planner's copy ranges complement, so it is read here per encoded tile.
    fn expand(
        ctx: &ArtifactPartitionCtx<'_>,
        items: Vec<PartitionItem>,
        out: &mut Vec<(u64, Served)>,
    ) {
        for item in items {
            match item {
                PartitionItem::RunCopy {
                    tile_id,
                    run_length,
                    ..
                } => {
                    let start = tile_id.raw();
                    for id in start..start + u64::from(run_length) {
                        out.push((id, Served::Copied));
                    }
                }
                PartitionItem::Encoded(tile) => out.push((
                    tile.tile_id.raw(),
                    Served::Encoded {
                        spliced: ctx.splice_run(tile.tile_id).is_some(),
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
                ctx,
                plan_artifact_items(ctx, &mut cursor, chunk, false).expect("plan batch"),
                out,
            );
        }
        expand(
            ctx,
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
        // The artifact payload here is opaque, not gzip: a tile that does
        // not survive must never read it.
        assert!(
            encode_partition_batch(&pending, MVT_GZIP_6, Some(&ctx))
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
            MVT_GZIP_6,
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

    fn feature_record(geom_type: crate::mvt::GeomType, geometry: &[u32]) -> Box<[u8]> {
        crate::wire_format::encode_feature_data_with_attrs(1, geom_type, geometry, &[0])
    }

    fn line_record() -> Box<[u8]> {
        let mut geometry = Vec::new();
        crate::mvt::encode_linestring(&mut geometry, &[(100, 100), (900, 300)]);
        feature_record(crate::mvt::GeomType::LineString, &geometry)
    }

    fn polygon_record() -> Box<[u8]> {
        let mut geometry = Vec::new();
        crate::mvt::encode_polygon(
            &mut geometry,
            &[&[(0, 0), (4096, 0), (4096, 4096), (0, 4096), (0, 0)]],
        );
        feature_record(crate::mvt::GeomType::Polygon, &geometry)
    }

    /// A gzip MVT payload holding one ocean layer, as the artifact stores.
    fn ocean_payload() -> Vec<u8> {
        let mut layer = crate::mvt::LayerBuilder::new(Layer::Ocean.name());
        crate::wire_format::add_feature_to_layer(&mut layer, &polygon_record());
        let mut raw = Vec::new();
        crate::mvt::encode_tile_into(&mut raw, &[&layer], &mut crate::mvt::EncodeScratch::new());
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
        encoder.write_all(&raw).unwrap();
        encoder.finish().unwrap()
    }

    /// A world artifact holding `payload` at each of `addresses`, admitted
    /// with world bounds so the artifact owns every address.
    fn world_artifact(
        dir: &std::path::Path,
        addresses: &[TileId],
        payload: &[u8],
    ) -> crate::ocean::OceanTiles {
        let path = dir.join("ocean.pmtiles");
        let key = test_artifact_key();
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
            .unwrap(),
        );
        writer.set_ocean_only_metadata();
        writer.set_ocean_artifact_metadata(key.clone()).unwrap();
        for &address in addresses {
            writer.add_run(address, 1, payload).unwrap();
        }
        writer.write_to(&path).unwrap();
        let world = crate::geometry::projection::MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        crate::ocean::OceanArtifact::admit(&path, 6, || Ok(key.clone()))
            .and_then(|artifact| artifact.into_tiles(crate::ocean::OceanBand::for_bounds(&world)))
            .unwrap()
    }

    /// Appending the artifact's ocean field before the one compression must
    /// produce the exact payload the former splice produced by decompressing
    /// the OSM tile, appending the field and recompressing at the zoom's
    /// level. Zooms 2, 10 and 13 sit in the three compression-policy bands.
    #[test]
    fn appending_before_compression_matches_the_recompressing_splice() {
        let dir = tempfile::tempdir().unwrap();
        let addresses = [
            TileId::at(2, 1, 1),
            TileId::at(10, 3, 5),
            TileId::at(13, 7, 2),
        ];
        let payload = ocean_payload();
        let tiles = world_artifact(dir.path(), &addresses, &payload);
        let ctx = ArtifactPartitionCtx::new(&tiles).unwrap();
        let field = ocean_layer_field(&payload).unwrap();
        let pending = || -> Vec<PendingTile> {
            addresses
                .iter()
                .map(|&tile_id| PendingTile {
                    tile_id,
                    features: vec![(Layer::Boundaries, line_record())],
                })
                .collect()
        };

        let osm_only = encode_partition_batch(&pending(), MVT_GZIP_6, None).unwrap();
        let spliced = encode_partition_batch(&pending(), MVT_GZIP_6, Some(&ctx)).unwrap();
        assert_eq!(osm_only.len(), addresses.len());
        assert_eq!(spliced.len(), addresses.len());
        for (osm, spliced) in osm_only.iter().zip(&spliced) {
            let mut raw = crate::archive::gzip_decompress(&osm.compressed).unwrap();
            raw.extend_from_slice(&field);
            let level = super::super::super::compression_level_for_zoom(osm.tile_id.zoom(), 6);
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
            encoder.write_all(&raw).unwrap();
            let expected = encoder.finish().unwrap();
            assert_eq!(spliced.tile_id, osm.tile_id);
            assert_eq!(
                spliced.compressed,
                expected,
                "payload differs at z{}",
                osm.tile_id.zoom()
            );
        }
    }

    /// Computed ocean on an artifact-owned tile means producer and consumer
    /// disagree on ownership; appending would draw the ocean twice.
    #[test]
    fn computed_ocean_on_an_artifact_owned_tile_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let address = TileId::at(10, 3, 5);
        let tiles = world_artifact(dir.path(), &[address], &ocean_payload());
        let ctx = ArtifactPartitionCtx::new(&tiles).unwrap();
        let pending = [PendingTile {
            tile_id: address,
            features: vec![(Layer::Ocean, polygon_record())],
        }];
        let Err(error) = encode_partition_batch(&pending, MVT_GZIP_6, Some(&ctx)) else {
            panic!("computed ocean on an artifact-owned tile was accepted");
        };
        assert!(
            error
                .to_string()
                .contains("unexpectedly contains computed ocean"),
            "{error:?}"
        );
    }

    /// Run copies are MVT + gzip, so the encoder refuses any other contract
    /// beside an artifact before doing work, even for an empty batch.
    #[test]
    fn the_encoder_refuses_an_artifact_outside_mvt_gzip() {
        let dir = tempfile::tempdir().unwrap();
        let tiles = world_artifact(dir.path(), &[TileId::at(2, 1, 1)], &ocean_payload());
        let ctx = ArtifactPartitionCtx::new(&tiles).unwrap();
        for contract in [
            TileContract::Mvt {
                compression: TileCompression::Brotli,
                level: 6,
            },
            TileContract::Mlt,
        ] {
            assert!(
                encode_partition_batch(&[], contract, Some(&ctx)).is_err(),
                "{contract:?} accepted an artifact"
            );
        }
    }

    #[test]
    fn ocean_layer_field_is_the_whole_strict_payload_and_refuses_foreign_fields() {
        let gzip = |raw: &[u8]| {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(raw).expect("gzip raw tile");
            encoder.finish().expect("finish gzip")
        };
        let field = [26, 7, 10, 5, b'o', b'c', b'e', b'a', b'n'];
        assert_eq!(ocean_layer_field(&gzip(&field)).unwrap(), field);

        // A tile field other than a layer would make the payload more than
        // the layer field, so it is refused rather than skipped.
        let mut foreign = vec![8, 1];
        foreign.extend_from_slice(&field);
        assert!(ocean_layer_field(&gzip(&foreign)).is_err());

        let mut two_layers = field.to_vec();
        two_layers.extend_from_slice(&field);
        assert!(ocean_layer_field(&gzip(&two_layers)).is_err());
    }
}
