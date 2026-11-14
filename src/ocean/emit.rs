// Ocean emission: one pass over one shapefile into sort chunks.
//
// Reads records through `shapefile`, keeps those the data bounds and band
// want, quantizes them on the pass grid and clips them to the data bounds,
// unions the low-zoom pass, then descends the shared integer pyramid in
// parallel with rayon into the sort writer's spill coalescer.

use super::band::{self, OceanBand, data_bounds_rect, rect_contains};
use super::key::OceanShapefileHashes;
#[cfg(test)]
use super::plan::OceanPass;
use super::plan::PassWindow;
use super::policy::{FULL_FILL_FEATURE_ID, ocean_params};
use super::shapefile::Shapefile;
use crate::geometry::int_ocean::{
    IntEmitScratch, IntRect, Shape, Shapes, intersect_rect_into, normalize_into, quantize_polygon,
    shape_bbox,
};
use crate::geometry::polygon::Area;
use crate::geometry::projection::{MercBbox, Point};
use crate::geometry::pyramid::{
    PyramidCell, PyramidEmitKind, PyramidParams, PyramidScratch, PyramidZooms, RingCapError,
    emit_shape_pyramid, emit_shape_pyramid_cell, split_for_parallel,
};
use crate::mvt::GeomType;
use crate::pipeline::PipelineError;
use crate::shortbread::Layer;
use crate::sort::{self, SortWriter};
use crate::wire_format::{append_feature_data_with_attrs, encode_attrs_bytes};
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Ocean input stats
// ---------------------------------------------------------------------------
//
// Shapefile processing runs the low-zoom pass (simplified when configured,
// full-resolution otherwise) and the full-resolution z8+ pass. Its input-side
// counts accumulate into these process-global atomics and are drained at
// OCEAN_END by emit_ocean_counters.
// OceanAcc records its output at each spill flush because the phase12 sort
// snapshot predates the ocean pass. The output counters use
// payload bytes only, matching the other sort_layer_*_bytes counters; its
// accumulator byte budget also includes PayloadRef storage and is not a
// comparable measure. The input side records how many shapefile shapes were
// read, how many overlapped the data bounds, how many polygon pieces they
// parsed into, and how many shapefile bytes were mapped. Ocean is ~30% of wall
// and a top allocator, so this is the phase most worth a look.

struct OceanStats {
    shapes: AtomicU64,
    shapes_hit: AtomicU64,
    pieces: AtomicU64,
    shapefile_bytes: AtomicU64,
    records: AtomicU64,
    payload_bytes: AtomicU64,
    zoom_records: [AtomicU64; ZOOM_COUNT],
    zoom_payload_bytes: [AtomicU64; ZOOM_COUNT],
}

static OCEAN_STATS: OceanStats = OceanStats {
    shapes: AtomicU64::new(0),
    shapes_hit: AtomicU64::new(0),
    pieces: AtomicU64::new(0),
    shapefile_bytes: AtomicU64::new(0),
    records: AtomicU64::new(0),
    payload_bytes: AtomicU64::new(0),
    zoom_records: [const { AtomicU64::new(0) }; ZOOM_COUNT],
    zoom_payload_bytes: [const { AtomicU64::new(0) }; ZOOM_COUNT],
};

use crate::tiling::ZOOM_COUNT;

fn ocean_layer_stats_enabled() -> bool {
    crate::debug::layer_stats_enabled()
}

/// Discard ocean totals left by a previous failed sequential pipeline run.
pub(crate) fn reset_ocean_counters() {
    for counter in [
        &OCEAN_STATS.shapes,
        &OCEAN_STATS.shapes_hit,
        &OCEAN_STATS.pieces,
        &OCEAN_STATS.shapefile_bytes,
        &OCEAN_STATS.records,
        &OCEAN_STATS.payload_bytes,
    ]
    .into_iter()
    .chain(OCEAN_STATS.zoom_records.iter())
    .chain(OCEAN_STATS.zoom_payload_bytes.iter())
    {
        counter.store(0, Ordering::Relaxed);
    }
}

/// Drain accumulated ocean input counters to the sidecar. Called once at
/// OCEAN_END; emits nothing when no shapefile records were processed.
pub(crate) fn emit_ocean_counters() {
    use crate::debug::emit_counter_u64;
    let shapes = OCEAN_STATS.shapes.swap(0, Ordering::Relaxed);
    let shapes_hit = OCEAN_STATS.shapes_hit.swap(0, Ordering::Relaxed);
    let pieces = OCEAN_STATS.pieces.swap(0, Ordering::Relaxed);
    let shapefile_bytes = OCEAN_STATS.shapefile_bytes.swap(0, Ordering::Relaxed);
    let records = OCEAN_STATS.records.swap(0, Ordering::Relaxed);
    let payload_bytes = OCEAN_STATS.payload_bytes.swap(0, Ordering::Relaxed);
    let zoom_records: [u64; ZOOM_COUNT] =
        std::array::from_fn(|z| OCEAN_STATS.zoom_records[z].swap(0, Ordering::Relaxed));
    let zoom_payload_bytes: [u64; ZOOM_COUNT] =
        std::array::from_fn(|z| OCEAN_STATS.zoom_payload_bytes[z].swap(0, Ordering::Relaxed));
    if shapes == 0 {
        return;
    }
    emit_counter_u64("ocean_shapes", shapes);
    emit_counter_u64("ocean_shapes_hit", shapes_hit);
    emit_counter_u64("ocean_pieces", pieces);
    emit_counter_u64("ocean_shapefile_bytes", shapefile_bytes);

    if records == 0 {
        return;
    }
    emit_counter_u64("sort_layer_ocean_records", records);
    emit_counter_u64("sort_layer_ocean_bytes", payload_bytes);
    if !ocean_layer_stats_enabled() {
        return;
    }
    for (z, (&records, &bytes)) in zoom_records.iter().zip(&zoom_payload_bytes).enumerate() {
        if records > 0 {
            emit_counter_u64(&format!("sort_layer_ocean_z{z}_records"), records);
        }
        if bytes > 0 {
            emit_counter_u64(&format!("sort_layer_ocean_z{z}_bytes"), bytes);
        }
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

const LARGE_PIECE_VERTICES: usize = 1024;

/// Union pre-split source pieces into seam-free shapes: every contour of
/// every piece goes into one soup, and the overlay engine's nonzero Simplify
/// re-derives ring roles geometrically. Quantization gives adjacent cells
/// bit-identical shared-edge coordinates, so interior cell edges cancel
/// exactly.
///
/// INVARIANT: nonzero composition of a contour soup equals set union only
/// when piece interiors are disjoint - the mosaic property the split source
/// data guarantees (cells partition the plane; each piece's holes lie inside
/// its own outer). If two records' interiors overlapped, an outer from one
/// could cancel against a hole from the other under summed winding where a
/// Boolean OR would stay filled. Overlapping source records are an input
/// defect, not a case this function handles.
fn union_pieces(pieces: Vec<Shape>) -> Vec<Shape> {
    if pieces.len() <= 1 {
        return pieces;
    }
    let contour_count: usize = pieces.iter().map(Vec::len).sum();
    let mut soup: Shape = Vec::with_capacity(contour_count);
    for mut piece in pieces {
        soup.append(&mut piece);
    }
    let mut scratch = IntEmitScratch::new();
    let mut merged: Shapes = Vec::new();
    normalize_into(&mut scratch, soup, Area::ZERO, &mut merged);
    merged
}
// Cap each parallel fold accumulator's in-flight payload well below the
// global sort_budget: with (piece x zoom) fan-out across many rayon workers,
// letting each balloon to the full budget before flushing would multiply peak
// RSS by the worker count. A flush is a memcpy into the shared spill
// coalescer (which owns chunk sizing), so small thresholds are cheap - the
// old direct-to-disk flushes at this size were the main source of the NA
// chunk fragmentation (2754 chunks, fan-in 1076; see sort::SpillCoalescer).
const OCEAN_CHUNK_SIZE_LIMIT: usize = 4 * 1024 * 1024;

struct ParsedOceanRecord {
    pieces: Vec<Shape>,
    shapes_hit: u64,
}

// Pyramid recursion scratch is built per work item, NOT pooled per thread: a
// thread-local fragment pool grows to the fattest piece each rayon thread ever
// descends and retains it for the whole phase. Only IntEmitScratch is reused by
// a rayon worker thread. Its overlay buffers are bounded by one operation, and
// root/rescale inputs are drawn from its ring and shape pools, so the pools do
// not grow once the worker is warm.
thread_local! {
    static OCEAN_INT_SCRATCH: std::cell::RefCell<IntEmitScratch> =
        std::cell::RefCell::new(IntEmitScratch::new());
}

enum OceanWorkKind {
    Whole(usize),
    Cell(PyramidCell, Shapes),
}

struct OceanWorkItem {
    feature_id: u64,
    kind: OceanWorkKind,
}

struct OceanAcc {
    records: Vec<sort::PayloadRef>,
    payload: Vec<u8>,
    bytes: usize,
    count: u64,
}

impl OceanAcc {
    fn new() -> Self {
        Self {
            records: Vec::new(),
            payload: Vec::new(),
            bytes: 0,
            count: 0,
        }
    }

    fn flush(&mut self, spill: &sort::SpillCoalescer) {
        if self.records.is_empty() {
            return;
        }
        spill.append(&self.records, &self.payload);
        OCEAN_STATS
            .records
            .fetch_add(self.records.len() as u64, Ordering::Relaxed);
        OCEAN_STATS
            .payload_bytes
            .fetch_add(self.payload.len() as u64, Ordering::Relaxed);
        if ocean_layer_stats_enabled() {
            for record in &self.records {
                // Keys are minted for profile zooms only (SortKey::new).
                let zoom = usize::from(record.key.tile().zoom());
                OCEAN_STATS.zoom_records[zoom].fetch_add(1, Ordering::Relaxed);
                OCEAN_STATS.zoom_payload_bytes[zoom]
                    .fetch_add(record.len as u64, Ordering::Relaxed);
            }
        }
        self.count += self.records.len() as u64;
        self.records.clear();
        self.payload.clear();
        self.bytes = 0;
    }

    fn merge_from(&mut self, other: Self) {
        self.count += other.count;

        let payload_base = self.payload.len();
        self.payload.extend(other.payload);
        self.records
            .extend(other.records.into_iter().map(|r| sort::PayloadRef {
                offset: payload_base + r.offset,
                ..r
            }));
        self.bytes += other.bytes;
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Process synthetic test inputs without a pre-resolved source key. The
/// window must lie inside one pass.
#[cfg(test)]
#[allow(clippy::too_many_lines, clippy::unwrap_in_result)]
#[hotpath::measure]
pub(crate) fn process_ocean_shapefile(
    path: &std::path::Path,
    data_bounds: &MercBbox,
    min_zoom: u8,
    max_zoom: u8,
    sort_writer: &mut SortWriter,
    band: Option<&OceanBand>,
) -> Result<u64, PipelineError> {
    let window = crate::tiling::ZoomRange::new(min_zoom, max_zoom)
        .and_then(|zooms| OceanPass::for_zoom(min_zoom).emission(zooms))
        .filter(|window| window.zooms().last() == max_zoom)
        .expect("test window lies inside one ocean pass");
    process_ocean_shapefile_inner(path, data_bounds, window, sort_writer, band, false, None)
        .map(|(count, _)| count)
}

/// Process a shapefile and return hashes of the source buffers used.
pub(crate) fn process_ocean_shapefile_with_hashes(
    path: &std::path::Path,
    data_bounds: &MercBbox,
    window: PassWindow,
    sort_writer: &mut SortWriter,
) -> Result<(u64, OceanShapefileHashes), PipelineError> {
    let (count, hashes) =
        process_ocean_shapefile_inner(path, data_bounds, window, sort_writer, None, true, None)?;
    let hashes = hashes.ok_or_else(|| {
        PipelineError::failed("ocean shapefile processing did not capture source hashes")
    })?;
    Ok((count, hashes))
}

/// Process a shapefile only if the buffers opened for this pass match the
/// source key resolved before phase12. The comparison happens before parsing
/// or emitting records, so a replaced input cannot stamp mixed chunks with
/// the earlier key.
#[hotpath::measure]
pub(crate) fn process_ocean_shapefile_checked(
    path: &std::path::Path,
    data_bounds: &MercBbox,
    window: PassWindow,
    sort_writer: &mut SortWriter,
    band: Option<&OceanBand>,
    expected_hashes: OceanShapefileHashes,
) -> Result<u64, PipelineError> {
    let (count, _) = process_ocean_shapefile_inner(
        path,
        data_bounds,
        window,
        sort_writer,
        band,
        true,
        Some(expected_hashes),
    )?;
    Ok(count)
}

/// Run one ocean pass over one shapefile.
///
/// Construction is canonical: pieces are quantized on the pass grid, roots
/// are chosen at the pass's first zoom and descent follows the pass, so the
/// window's zooms only decide which constructed zooms are written. Descent
/// stops at the window's last zoom, which cannot change a shallower zoom, and
/// the sink drops zooms shallower than its first. `band`,
/// when an artifact serves the run, prunes the cells the artifact owns.
#[allow(clippy::too_many_lines, clippy::unwrap_in_result)]
fn process_ocean_shapefile_inner(
    path: &std::path::Path,
    data_bounds: &MercBbox,
    window: PassWindow,
    sort_writer: &mut SortWriter,
    band: Option<&OceanBand>,
    capture_hashes: bool,
    expected_hashes: Option<OceanShapefileHashes>,
) -> Result<(u64, Option<OceanShapefileHashes>), PipelineError> {
    let pass = window.pass();
    let grid_zoom = pass.grid_zoom();
    if band.is_some_and(OceanBand::is_empty) {
        eprintln!("  Ocean band is empty; skipping shapefile geometry");
        return Ok((0, None));
    }
    eprintln!("  Opening {}", path.display());

    // Opening validates the .shx index before the .shp is opened; the hash
    // comparison follows and precedes any record parsing.
    let shapefile = Shapefile::open(path)?;
    let shape_count = shapefile.records().len();
    // Hash the exact mapped bytes and index buffer consumed here. Artifact
    // builds record them in the key; regular runs compare them with the key
    // resolved before phase12.
    let hashes = capture_hashes.then(|| shapefile.hashes());
    if let Some(expected) = expected_hashes {
        let actual = hashes.expect("checked processing captures source hashes");
        if actual != expected {
            return Err(PipelineError::failed(format!(
                "ocean shapefile inputs changed after source-key resolution for {}: expected .shp/.shx hashes {}/{}, opened {}/{}",
                path.display(),
                expected.shp_xxh128,
                expected.shx_xxh128,
                actual.shp_xxh128,
                actual.shx_xxh128,
            )));
        }
    }
    shapefile.advise_random()?;
    eprintln!(
        "  Mmapped {:.1} MB",
        shapefile.mapped_len() as f64 / (1024.0 * 1024.0)
    );
    OCEAN_STATS.shapefile_bytes.fetch_add(
        u64::try_from(shapefile.mapped_len()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );

    let data_rect = data_bounds_rect(data_bounds, grid_zoom);

    // --- Parse phase: extract and clip polygons in parallel ---
    use rayon::prelude::*;

    let parsed_records: Result<Vec<ParsedOceanRecord>, std::io::Error> = shapefile
        .records()
        .par_iter()
        .map_init(IntEmitScratch::new, |scratch, &record| {
            // A record's pieces are local until it reads cleanly: on error
            // the whole collection fails and nothing escapes.
            let mut pieces = Vec::new();
            let hit = shapefile.read_polygons(
                record,
                |merc_min, merc_max| record_wanted(merc_min, merc_max, data_bounds, band, pass),
                |outer, inners| {
                    push_quantized_pieces(
                        scratch,
                        &mut pieces,
                        outer,
                        inners,
                        grid_zoom,
                        data_rect,
                    );
                },
            )?;
            Ok(ParsedOceanRecord {
                pieces,
                shapes_hit: u64::from(hit),
            })
        })
        .collect();
    let parsed_records = parsed_records?;
    shapefile.release()?;
    let shapes_hit: u64 = parsed_records.iter().map(|record| record.shapes_hit).sum();
    let parsed_pieces: usize = parsed_records
        .iter()
        .map(|record| record.pieces.len())
        .sum();

    let mut pieces: Vec<Shape> = Vec::with_capacity(parsed_pieces);
    for record in parsed_records {
        pieces.extend(record.pieces);
    }

    OCEAN_STATS.shapes.fetch_add(
        u64::try_from(shape_count).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    OCEAN_STATS
        .shapes_hit
        .fetch_add(shapes_hit, Ordering::Relaxed);
    // `ocean_pieces` keeps its documented meaning: parsed input pieces,
    // counted before the low-zoom union below.
    OCEAN_STATS.pieces.fetch_add(
        u64::try_from(pieces.len()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );

    // The source shapefiles arrive pre-split into grid cells, and each cell
    // used to descend the pyramid independently: adjacent cells simplified
    // their shared coastline into disagreeing shapes (seam wedges) and the
    // per-zoom min-area drop applied per cell fragment, deleting the smaller
    // half of any landform straddling a cell edge (half-peninsula spikes).
    // For the low-zoom pass the split grid is comparable to the tile size,
    // which made both artifacts obvious at z2, so its pieces are unioned into
    // seam-free shapes before descent. The z8-z14 full-polygons pass keeps
    // per-piece descent: its fixed pixel tolerances keep any seam mismatch
    // sub-pixel, and a world-scale union of the full polygons would dominate
    // `ocean-build`. Union input order is deterministic (rayon's indexed
    // collect preserves record order) and the overlay step is serial, so
    // emitted bytes stay deterministic.
    if pass.unions() {
        let union_start = std::time::Instant::now();
        let input_pieces = pieces.len();
        let input_vertices: usize = pieces.iter().map(total_vertices).sum();
        pieces = union_pieces(pieces);
        let output_vertices: usize = pieces.iter().map(total_vertices).sum();
        let union_elapsed = union_start.elapsed();
        eprintln!(
            "  Union: {input_pieces} pieces ({input_vertices} vertices) -> {} shapes ({output_vertices} vertices) in {union_elapsed:.2?}",
            pieces.len()
        );
        use crate::debug::emit_counter_u64;
        emit_counter_u64(
            "ocean_union_ns",
            u64::try_from(union_elapsed.as_nanos()).unwrap_or(u64::MAX),
        );
        emit_counter_u64(
            "ocean_union_input_pieces",
            u64::try_from(input_pieces).unwrap_or(u64::MAX),
        );
        emit_counter_u64(
            "ocean_union_input_vertices",
            u64::try_from(input_vertices).unwrap_or(u64::MAX),
        );
        emit_counter_u64(
            "ocean_union_shapes",
            u64::try_from(pieces.len()).unwrap_or(u64::MAX),
        );
        emit_counter_u64(
            "ocean_union_output_vertices",
            u64::try_from(output_vertices).unwrap_or(u64::MAX),
        );
    }

    let poly_count = pieces.len();
    eprintln!(
        "  {shape_count} shapes, {shapes_hit} in bounds, {poly_count} polygons - processing in parallel"
    );

    // --- Process phase: parallel with rayon, direct chunk flushing ---
    //
    // Each rayon worker accumulates records in a thread-local buffer and flushes
    // directly to a chunk file when the buffer exceeds chunk_size_bytes. This
    // avoids holding all ocean sort records in memory simultaneously - at planet
    // scale that could be 10-30 GB. The previous approach (par_iter().collect()
    // into Vec<Vec<SortRecord>> + serial push) was fine for regional extracts but
    // would blow memory and serialize sort+flush at planet scale.
    let mut empty_attrs_bytes = Vec::new();
    encode_attrs_bytes(&mut empty_attrs_bytes, &[], grid_zoom);

    // Ocean chunks share the current partitioned filename grammar and chunk
    // ID sequence with PBF chunks, so resume scans find the complete set.
    let spill = sort::SpillCoalescer::new(sort_writer.chunk_sink(), sort_writer.chunk_size_bytes());
    let chunk_size = sort_writer.chunk_size_bytes().min(OCEAN_CHUNK_SIZE_LIMIT);

    // The traversal filter is band ownership only. The emission window must
    // not join it: a pruned cell loses its whole subtree, so filtering zooms
    // above the window here would drop every requested zoom below it.
    let tile_filter =
        |cell: PyramidCell| band.is_none_or(|band| band.computed_owns(cell.z, cell.tx, cell.ty));
    let tile_filter_dyn: Option<&(dyn Fn(PyramidCell) -> bool + Sync)> =
        band.map(|_| &tile_filter as &(dyn Fn(PyramidCell) -> bool + Sync));
    let zooms = PyramidZooms::new(grid_zoom, pass.first_zoom(), window.zooms().last())
        .expect("a pass window lies inside its pass grid");
    let params = ocean_params(zooms, tile_filter_dyn);
    let first_emitted = window.zooms().first();
    // Per-piece item target for the parallel frontier. Root cells of a
    // large piece usually exceed this on their own (the frontier loop is
    // then a no-op); it only forces expansion for single-root ranges.
    const SPLIT_ITEMS_PER_PIECE: usize = 16;
    // Split phase runs in parallel across pieces; each large piece emits
    // its above-frontier tiles into its own accumulator, merged (and
    // flushed) serially afterwards.
    type PiecePrep = (Vec<OceanWorkItem>, Option<OceanAcc>);
    let piece_prep: Result<Vec<PiecePrep>, RingCapError> = pieces
        .par_iter()
        .enumerate()
        .map(|(piece_idx, piece)| {
            let feature_id = piece_idx as u64;
            if total_vertices(piece) < LARGE_PIECE_VERTICES {
                return Ok((
                    vec![OceanWorkItem {
                        feature_id,
                        kind: OceanWorkKind::Whole(piece_idx),
                    }],
                    None,
                ));
            }
            let mut acc = OceanAcc::new();
            let cells = {
                let mut scratch = PyramidScratch::new();
                let mut sink = ocean_sink(feature_id, &empty_attrs_bytes, first_emitted, &mut acc);
                split_for_parallel(
                    piece,
                    &params,
                    SPLIT_ITEMS_PER_PIECE,
                    &mut scratch,
                    &mut sink,
                )?
            };
            if acc.bytes >= chunk_size {
                acc.flush(&spill);
            }
            let items = cells
                .into_iter()
                .map(|(cell, frag)| OceanWorkItem {
                    feature_id,
                    kind: OceanWorkKind::Cell(cell, frag),
                })
                .collect();
            Ok((items, Some(acc)))
        })
        .collect();

    let mut work_items = Vec::with_capacity(pieces.len());
    let mut pre_emit = OceanAcc::new();
    for (items, acc) in piece_prep? {
        work_items.extend(items);
        if let Some(acc) = acc {
            pre_emit.merge_from(acc);
            if pre_emit.bytes >= chunk_size {
                pre_emit.flush(&spill);
            }
        }
    }

    // A failed item fails the pass: try_fold stops scheduling new items, and
    // records already spilled belong to a run that is returning an error.
    let mut result = work_items
        .into_par_iter()
        .try_fold(OceanAcc::new, |mut acc, item| {
            emit_ocean_piece(
                item,
                &pieces,
                &params,
                &empty_attrs_bytes,
                first_emitted,
                &mut acc,
            )?;
            if acc.bytes >= chunk_size {
                acc.flush(&spill);
            }
            Ok::<_, RingCapError>(acc)
        })
        .try_reduce(OceanAcc::new, |mut a, b| {
            a.merge_from(b);
            if a.bytes >= chunk_size {
                a.flush(&spill);
            }
            Ok(a)
        })?;

    result.merge_from(pre_emit);
    result.flush(&spill);
    // Every producer above has completed, so the verdict is final.
    let paths = spill
        .finish()
        .map_err(|err| PipelineError::io("ocean sort chunk write failed", err))?;
    sort_writer.adopt_chunk_files(paths);
    let count = result.count;

    eprintln!("  {poly_count} polygons, {count} features");
    Ok((count, hashes))
}

fn push_quantized_pieces(
    scratch: &mut IntEmitScratch,
    pieces: &mut Vec<Shape>,
    outer: &[Point],
    inners: &[Vec<Point>],
    max_zoom: u8,
    data_rect: IntRect,
) {
    let shape = quantize_polygon(outer, inners, max_zoom);
    if shape.is_empty() {
        return;
    }

    // The common case for an in-bounds extract: the shape lies entirely
    // inside the data bounds - the boolean is an expensive identity.
    if shape_bbox(&shape).is_some_and(|bb| rect_contains(data_rect, bb)) {
        pieces.push(shape);
        return;
    }

    let mut clipped = Vec::new();
    intersect_rect_into(scratch, &shape, data_rect, Area::ZERO, &mut clipped);
    for piece in clipped {
        pieces.push(piece);
    }
}

/// Does this pass want a record with these projected bounds? It must overlap
/// the data bounds, and when an artifact serves the run, reach a root range
/// the computed side owns on this pass.
fn record_wanted(
    merc_min: Point,
    merc_max: Point,
    data_bounds: &MercBbox,
    band: Option<&OceanBand>,
    pass: super::plan::OceanPass,
) -> bool {
    if merc_max.x < data_bounds.min_x
        || merc_min.x > data_bounds.max_x
        || merc_max.y < data_bounds.min_y
        || merc_min.y > data_bounds.max_y
    {
        return false;
    }
    band.is_none_or(|band| {
        band::pass_owns_root_range(merc_min.x, merc_min.y, merc_max.x, merc_max.y, band, pass)
    })
}

fn total_vertices(piece: &Shape) -> usize {
    piece.iter().map(Vec::len).sum()
}

/// The record sink for one piece. Zooms shallower than `first_emitted`, the
/// window's first zoom, are constructed but not written.
fn ocean_sink<'a>(
    feature_id: u64,
    attrs_bytes: &'a [u8],
    first_emitted: u8,
    acc: &'a mut OceanAcc,
) -> impl FnMut(PyramidCell, &[u32], PyramidEmitKind) + 'a {
    move |cell: PyramidCell, geom: &[u32], kind: PyramidEmitKind| {
        if cell.z < first_emitted {
            return;
        }
        let tile_id = crate::tiling::TileId::at(cell.z, cell.tx, cell.ty);
        let key = sort::SortKey::new(tile_id, Layer::Ocean, 0);
        let range = append_feature_data_with_attrs(
            &mut acc.payload,
            if kind == PyramidEmitKind::FullFill {
                FULL_FILL_FEATURE_ID
            } else {
                feature_id
            },
            GeomType::Polygon,
            geom,
            attrs_bytes,
        );
        acc.bytes += range.len() + std::mem::size_of::<sort::PayloadRef>();
        acc.records.push(sort::PayloadRef {
            key,
            offset: range.start,
            len: range.len(),
        });
    }
}

#[hotpath::measure]
fn emit_ocean_piece(
    item: OceanWorkItem,
    pieces: &[Shape],
    params: &PyramidParams<'_>,
    attrs_bytes: &[u8],
    first_emitted: u8,
    acc: &mut OceanAcc,
) -> Result<(), RingCapError> {
    OCEAN_INT_SCRATCH.with(|int_cell| {
        let mut int_scratch = int_cell.borrow_mut();
        let mut scratch = PyramidScratch::new();
        std::mem::swap(&mut scratch.int, &mut *int_scratch);
        let mut sink = ocean_sink(item.feature_id, attrs_bytes, first_emitted, acc);
        let result = match item.kind {
            OceanWorkKind::Whole(piece_idx) => {
                emit_shape_pyramid(&pieces[piece_idx], params, &mut scratch, &mut sink)
            }
            OceanWorkKind::Cell(cell, frag) => {
                emit_shape_pyramid_cell(cell, frag, params, &mut scratch, &mut sink)
            }
        };
        std::mem::swap(&mut scratch.int, &mut *int_scratch);
        result
    })
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::key::Xxh128;
    use super::super::shapefile::test_files::*;
    use super::*;
    use crate::geometry::int_ocean::IntPoint;
    use crate::geometry::pyramid::buffered_cell_rect_base;
    use std::fs;
    use std::path::Path;

    const Z0_CELL: PyramidCell = PyramidCell { z: 0, tx: 0, ty: 0 };

    fn record_osm_id(data: &[u8]) -> u64 {
        // The sort record payload leads with the raw little-endian osm_id
        // (wire_format.rs layout), not a protobuf message.
        u64::from_le_bytes(data[0..8].try_into().expect("record osm_id"))
    }

    #[test]
    fn overlapping_piece_zero_fragment_and_full_fill_have_distinct_ids() {
        let cell = PyramidCell {
            z: 8,
            tx: 100,
            ty: 100,
        };
        let rect = buffered_cell_rect_base(8, cell);
        let rectangle = |rect: IntRect| {
            vec![vec![
                IntPoint::new(rect.min_x, rect.min_y),
                IntPoint::new(rect.max_x, rect.min_y),
                IntPoint::new(rect.max_x, rect.max_y),
                IntPoint::new(rect.min_x, rect.max_y),
            ]]
        };
        // Piece zero crosses the tile edge, while piece one overlaps it by
        // only a narrow strip and covers the complete buffered tile.
        let piece_zero = rectangle(IntRect {
            min_x: rect.min_x - 3_000,
            min_y: rect.min_y + 1_000,
            max_x: rect.min_x + 240,
            max_y: rect.min_y + 1_040,
        });
        let piece_one = rectangle(IntRect {
            min_x: rect.min_x - 2_048,
            min_y: rect.min_y - 2_048,
            max_x: rect.max_x + 2_048,
            max_y: rect.max_y + 2_048,
        });
        let params = ocean_params(PyramidZooms::new(8, 8, 8).unwrap(), None);
        let mut attrs = Vec::new();
        encode_attrs_bytes(&mut attrs, &[], 8);
        let tile_id = crate::tiling::TileId::at(cell.z, cell.tx, cell.ty);

        let mut fragment_acc = OceanAcc::new();
        let mut fragment_kinds = Vec::new();
        let mut fragment_sink = ocean_sink(0, &attrs, 0, &mut fragment_acc);
        let mut scratch = PyramidScratch::new();
        emit_shape_pyramid(
            &piece_zero,
            &params,
            &mut scratch,
            &mut |at: PyramidCell, geom, kind| {
                if at == cell {
                    fragment_kinds.push(kind);
                }
                fragment_sink(at, geom, kind);
            },
        )
        .unwrap();
        drop(fragment_sink);

        let mut fill_acc = OceanAcc::new();
        let mut fill_kinds = Vec::new();
        let mut fill_sink = ocean_sink(1, &attrs, 0, &mut fill_acc);
        let mut scratch = PyramidScratch::new();
        emit_shape_pyramid(
            &piece_one,
            &params,
            &mut scratch,
            &mut |at: PyramidCell, geom, kind| {
                if at == cell {
                    fill_kinds.push(kind);
                }
                fill_sink(at, geom, kind);
            },
        )
        .unwrap();
        drop(fill_sink);

        assert!(fragment_kinds.contains(&PyramidEmitKind::Fragment));
        assert!(fill_kinds.contains(&PyramidEmitKind::FullFill));
        let id_for_tile = |acc: &OceanAcc| {
            acc.records.iter().find_map(|r| {
                (r.key.tile() == tile_id).then(|| record_osm_id(r.bytes(&acc.payload)))
            })
        };
        assert_eq!(id_for_tile(&fragment_acc), Some(0));
        assert_eq!(id_for_tile(&fill_acc), Some(FULL_FILL_FEATURE_ID));
    }

    #[test]
    fn full_fill_uses_canonical_id_and_fragment_keeps_piece_id() {
        let mut acc = OceanAcc::new();
        let attrs = Vec::new();
        let mut sink = ocean_sink(77, &attrs, 0, &mut acc);
        sink(Z0_CELL, &[9, 0, 0], PyramidEmitKind::FullFill);
        sink(Z0_CELL, &[9, 0, 0], PyramidEmitKind::Fragment);
        drop(sink);
        let first = acc.records[0];
        assert_eq!(
            record_osm_id(first.bytes(&acc.payload)),
            FULL_FILL_FEATURE_ID
        );
        assert_eq!(record_osm_id(acc.records[1].bytes(&acc.payload)), 77);
        let mut sink = ocean_sink(88, &attrs, 0, &mut acc);
        sink(Z0_CELL, &[9, 0, 0], PyramidEmitKind::FullFill);
        drop(sink);
        assert_eq!(
            first.bytes(&acc.payload),
            acc.records[2].bytes(&acc.payload),
            "full fills from different pieces retain identical payloads"
        );
    }

    #[test]
    fn piece_zero_fragment_round_trips_as_explicit_some_zero_id() {
        let mut acc = OceanAcc::new();
        // The decode side expects the encoded attrs-table form, exactly as
        // process_ocean_shapefile builds it - a bare empty Vec parses as a
        // truncated record and add_feature_to_layer rejects it.
        let mut attrs = Vec::new();
        encode_attrs_bytes(&mut attrs, &[], 14);
        let mut sink = ocean_sink(0, &attrs, 0, &mut acc);
        sink(Z0_CELL, &[9, 0, 0], PyramidEmitKind::Fragment);
        drop(sink);
        let data = acc.records[0].bytes(&acc.payload);
        let mut layer = crate::mvt::LayerBuilder::new("ocean");
        crate::wire_format::add_feature_to_layer(&mut layer, data).unwrap();
        assert_eq!(layer.features()[0].id, Some(0));

        let mut encoded = Vec::new();
        let mut scratch = crate::mvt::EncodeScratch::new();
        crate::mvt::encode_tile_into(&mut encoded, &[&layer], &mut scratch);
        // Feature id 0 is present as the explicit varint field form, not
        // elided as a protobuf default.
        assert!(encoded.windows(2).any(|bytes| bytes == [8, 0]));
    }

    fn merc_square(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Vec<Point> {
        vec![
            Point { x: min_x, y: min_y },
            Point { x: max_x, y: min_y },
            Point { x: max_x, y: max_y },
            Point { x: min_x, y: max_y },
        ]
    }

    #[test]
    fn union_pieces_merges_adjacent_cells_and_cancels_shared_edge() {
        use crate::geometry::int_ocean::signed_area_2x;
        // Two cells sharing the x=0.5 edge, as the pre-split source data
        // delivers them. Quantization puts the shared edge on bit-identical
        // coordinates, so the union must merge them into one seam-free shape
        // whose area is exactly the sum.
        let a = quantize_polygon(&merc_square(0.25, 0.25, 0.5, 0.5), &[], 7);
        let b = quantize_polygon(&merc_square(0.5, 0.25, 0.75, 0.5), &[], 7);
        let sum =
            signed_area_2x(&a[0]).get().unsigned_abs() + signed_area_2x(&b[0]).get().unsigned_abs();
        let merged = union_pieces(vec![a, b]);
        assert_eq!(merged.len(), 1, "adjacent cells must merge into one shape");
        assert_eq!(merged[0].len(), 1, "no holes expected");
        assert_eq!(signed_area_2x(&merged[0][0]).get().unsigned_abs(), sum);
    }

    #[test]
    fn union_of_split_pieces_restores_unsplit_pyramid_emission() {
        // The half-peninsula defect, end to end through pyramid emission. A
        // bump straddles the split edge x=0.5, sized so each half is ~200
        // z0-px^2 (under ocean_min_area's 256) while the whole is ~400 (over):
        // per-piece descent drops both halves at z0, the unioned pieces must
        // emit exactly what the unsplit shape emits at every zoom.
        let maxz = 2_u8;
        let w = 20.0 / 4096.0; // 20 z0 pixels in merc units
        let half = w / 2.0;
        let p = |x: f64, y: f64| Point { x, y };
        let s_unsplit = quantize_polygon(
            &[
                p(0.25, 0.25),
                p(0.5 - half, 0.25),
                p(0.5 - half, 0.25 - w),
                p(0.5 + half, 0.25 - w),
                p(0.5 + half, 0.25),
                p(0.75, 0.25),
                p(0.75, 0.5),
                p(0.25, 0.5),
            ],
            &[],
            maxz,
        );
        let piece_a = quantize_polygon(
            &[
                p(0.25, 0.25),
                p(0.5 - half, 0.25),
                p(0.5 - half, 0.25 - w),
                p(0.5, 0.25 - w),
                p(0.5, 0.5),
                p(0.25, 0.5),
            ],
            &[],
            maxz,
        );
        let piece_b = quantize_polygon(
            &[
                p(0.5, 0.25 - w),
                p(0.5 + half, 0.25 - w),
                p(0.5 + half, 0.25),
                p(0.75, 0.25),
                p(0.75, 0.5),
                p(0.5, 0.5),
            ],
            &[],
            maxz,
        );

        type Emitted = Vec<(u8, u32, u32, Vec<u32>)>;
        let emit_shapes = |shapes: &[Shape]| -> Emitted {
            let params = ocean_params(PyramidZooms::new(maxz, 0, maxz).unwrap(), None);
            let mut out: Emitted = Vec::new();
            let mut sink = |cell: PyramidCell, geom: &[u32], _kind: PyramidEmitKind| {
                out.push((cell.z, cell.tx, cell.ty, geom.to_vec()));
            };
            let mut scratch = PyramidScratch::new();
            for shape in shapes {
                emit_shape_pyramid(shape, &params, &mut scratch, &mut sink).unwrap();
            }
            out
        };

        let reference = emit_shapes(&[s_unsplit]);
        let per_piece = emit_shapes(&[piece_a.clone(), piece_b.clone()]);
        let unioned = emit_shapes(&union_pieces(vec![piece_a, piece_b]));

        // Calibration, both directions: per-piece emission must actually
        // exhibit the defect at z0 (bump halves dropped), or this test proves
        // nothing about the fix.
        let z0 = |emitted: &Emitted| -> Vec<Vec<u32>> {
            emitted
                .iter()
                .filter(|(z, _, _, _)| *z == 0)
                .map(|(_, _, _, geom)| geom.clone())
                .collect()
        };
        assert_ne!(
            z0(&reference),
            z0(&per_piece),
            "per-piece z0 emission must differ from unsplit (the defect)"
        );
        assert_eq!(
            reference, unioned,
            "unioned pieces must emit exactly the unsplit shape's pyramid"
        );
    }

    #[test]
    fn union_pieces_partial_shared_edge_with_extra_segmentation() {
        use crate::geometry::int_ocean::signed_area_2x;
        // B shares only part of A's right edge and carries an extra collinear
        // vertex on the shared segment - the split source's real shape: cell
        // edges meet with differing vertex segmentation and partial extent.
        let a = quantize_polygon(&merc_square(0.25, 0.25, 0.5, 0.5), &[], 2);
        let b = quantize_polygon(
            &[
                Point { x: 0.5, y: 0.3 },
                Point { x: 0.75, y: 0.3 },
                Point { x: 0.75, y: 0.45 },
                Point { x: 0.5, y: 0.45 },
                Point { x: 0.5, y: 0.375 },
            ],
            &[],
            2,
        );
        let sum =
            signed_area_2x(&a[0]).get().unsigned_abs() + signed_area_2x(&b[0]).get().unsigned_abs();
        let merged = union_pieces(vec![a, b]);
        assert_eq!(merged.len(), 1, "partial-edge neighbours must merge");
        assert_eq!(merged[0].len(), 1);
        assert_eq!(signed_area_2x(&merged[0][0]).get().unsigned_abs(), sum);
    }

    #[test]
    fn union_pieces_reassembles_island_split_across_cells() {
        // An island straddling the cell edge arrives as boundary notches in
        // both cells' outer rings (a clipped hole touching the clip edge is a
        // notch, not a hole). The union must close it back into a real hole.
        let a = quantize_polygon(
            &[
                Point { x: 0.25, y: 0.25 },
                Point { x: 0.5, y: 0.25 },
                Point { x: 0.5, y: 0.35 },
                Point { x: 0.45, y: 0.35 },
                Point { x: 0.45, y: 0.4 },
                Point { x: 0.5, y: 0.4 },
                Point { x: 0.5, y: 0.5 },
                Point { x: 0.25, y: 0.5 },
            ],
            &[],
            2,
        );
        let b = quantize_polygon(
            &[
                Point { x: 0.5, y: 0.25 },
                Point { x: 0.75, y: 0.25 },
                Point { x: 0.75, y: 0.5 },
                Point { x: 0.5, y: 0.5 },
                Point { x: 0.5, y: 0.4 },
                Point { x: 0.55, y: 0.4 },
                Point { x: 0.55, y: 0.35 },
                Point { x: 0.5, y: 0.35 },
            ],
            &[],
            2,
        );
        let merged = union_pieces(vec![a, b]);
        assert_eq!(merged.len(), 1, "the two cells must merge into one shape");
        assert_eq!(
            merged[0].len(),
            2,
            "the split island must come back as a hole"
        );
    }

    #[test]
    fn union_pieces_keeps_holes_and_disjoint_pieces() {
        let holed = quantize_polygon(
            &merc_square(0.1, 0.1, 0.4, 0.4),
            &[merc_square(0.2, 0.2, 0.3, 0.3)],
            7,
        );
        assert_eq!(holed.len(), 2);
        let solo = quantize_polygon(&merc_square(0.6, 0.6, 0.7, 0.7), &[], 7);
        let merged = union_pieces(vec![holed, solo]);
        assert_eq!(merged.len(), 2, "disjoint pieces must stay separate");
        let mut ring_counts: Vec<usize> = merged.iter().map(Vec::len).collect();
        ring_counts.sort_unstable();
        assert_eq!(ring_counts, vec![1, 2], "the hole must survive the union");
    }

    #[test]
    fn process_ocean_shapefile_emits_features_when_bbox_overlaps() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_test.shp");
        write_test_polygon_shapefile(&shp_path);

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };

        let emitted =
            process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer, None).unwrap();

        assert!(emitted > 0, "expected ocean features to be emitted");

        let mut reader = sort_writer.finish().unwrap();
        let mut count = 0u64;
        for partition in reader.take_partitions() {
            let mut pr =
                sort::SortPartitionReader::open(&partition, sort::ChunkCompression::None).unwrap();
            while pr.next().unwrap().is_some() {
                count += 1;
            }
        }
        assert_eq!(count, emitted, "emitted count should match sorted records");
    }

    fn sorted_records(
        dir: &Path,
        run: impl FnOnce(&mut SortWriter),
    ) -> Vec<(sort::SortKey, Box<[u8]>)> {
        let mut sort_writer = SortWriter::new(dir, 1 << 20, sort::ChunkCompression::None).unwrap();
        run(&mut sort_writer);
        let mut reader = sort_writer.finish().unwrap();
        let mut records = Vec::new();
        for partition in reader.take_partitions() {
            let mut pr =
                sort::SortPartitionReader::open(&partition, sort::ChunkCompression::None).unwrap();
            while let Some(record) = pr.next().unwrap() {
                records.push((record.key, record.data));
            }
        }
        records.sort();
        records
    }

    /// A narrowed window truncates ocean emission without moving
    /// construction: each pass still quantizes on its z7 or z14 grid and
    /// roots at its own first zoom, so a narrowed run's tiles are exactly a
    /// full build's tiles at those zooms.
    #[test]
    fn zoom_window_ocean_emission_equals_full_emission_restricted() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("island.shp");
        // A ragged ring about a z10 tile across near the map centre, small
        // enough to keep z14 descent to a few hundred tiles.
        let mut ring: Vec<(f64, f64)> = (0..48)
            .map(|i| {
                let angle = f64::from(i) * std::f64::consts::TAU / 48.0;
                let radius = 20_000.0 + 6_000.0 * (f64::from(i) * 1.7).sin();
                (
                    radius * angle.cos() + 3_000.0,
                    radius * angle.sin() - 2_000.0,
                )
            })
            .collect();
        ring.push(ring[0]);
        write_single_record_polygon_shapefile(&shp_path, &[ring]);
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let run_window = |dir: &Path, min_zoom: u8, max_zoom: u8| {
            sorted_records(dir, |writer| {
                for pass in OceanPass::ALL {
                    let zooms =
                        crate::tiling::ZoomRange::new(min_zoom, max_zoom).expect("test window");
                    if let Some(window) = pass.emission(zooms) {
                        process_ocean_shapefile_inner(
                            &shp_path, &bounds, window, writer, None, false, None,
                        )
                        .unwrap();
                    }
                }
            })
        };
        let full = run_window(&dir.path().join("full"), 0, crate::tiling::MAX_ZOOM);
        for (min_zoom, max_zoom) in [(3, 5), (0, 9), (6, 12), (10, 14), (5, 5)] {
            let narrowed = run_window(
                &dir.path().join(format!("w{min_zoom}-{max_zoom}")),
                min_zoom,
                max_zoom,
            );
            let restricted: Vec<_> = full
                .iter()
                .filter(|(key, _)| {
                    let zoom = key.tile().zoom();
                    min_zoom <= zoom && zoom <= max_zoom
                })
                .cloned()
                .collect();
            assert!(
                !narrowed.is_empty(),
                "window z{min_zoom}-z{max_zoom} emits ocean"
            );
            assert_eq!(
                narrowed, restricted,
                "window z{min_zoom}-z{max_zoom} must equal the full build there"
            );
        }
    }

    #[test]
    fn process_ocean_shapefile_emits_nothing_when_bbox_disjoint() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_test_disjoint.shp");
        write_test_polygon_shapefile(&shp_path);

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        // Mercator data bounds outside [0,1] should not intersect any valid projected shape.
        let disjoint_bounds = MercBbox {
            min_x: 2.0,
            min_y: 2.0,
            max_x: 3.0,
            max_y: 3.0,
        };

        let emitted =
            process_ocean_shapefile(&shp_path, &disjoint_bounds, 0, 0, &mut sort_writer, None)
                .unwrap();

        assert_eq!(emitted, 0, "expected no ocean features for disjoint bounds");
    }

    #[test]
    fn process_ocean_shapefile_handles_polygon_with_hole() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_test_hole.shp");
        write_test_polygon_with_hole_shapefile(&shp_path);

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };

        let emitted =
            process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer, None).unwrap();
        assert!(emitted > 0, "expected ocean features to be emitted");

        let mut reader = sort_writer.finish().unwrap();
        let partition = reader
            .take_partitions()
            .into_iter()
            .next()
            .expect("expected at least one partition");
        let rec = sort::SortPartitionReader::open(&partition, sort::ChunkCompression::None)
            .unwrap()
            .next()
            .unwrap()
            .expect("expected at least one record");
        // Walk the MVT command stream (after the 8-byte id, type byte and
        // u32 command count) and count rings by their MoveTo commands. A
        // single feature with two rings is the outer plus its hole; a hole
        // misread as an outer would union away at z0 and leave one ring.
        let cmd_count = u32::from_le_bytes(rec.data[9..13].try_into().unwrap()) as usize;
        let words: Vec<u32> = (0..cmd_count)
            .map(|i| u32::from_le_bytes(rec.data[13 + i * 4..17 + i * 4].try_into().unwrap()))
            .collect();
        let mut rings = 0;
        let mut i = 0;
        while i < words.len() {
            let (id, count) = (words[i] & 7, (words[i] >> 3) as usize);
            rings += usize::from(id == 1);
            i += 1 + if id == 7 { 0 } else { 2 * count };
        }
        assert_eq!(emitted, 1, "one z0 feature");
        assert_eq!(rings, 2, "the hole survives emission");
    }

    fn world() -> MercBbox {
        MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        }
    }

    #[test]
    fn an_empty_band_skips_without_opening_the_input() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.shp");
        let band = OceanBand::for_bounds(&world());
        assert!(band.is_empty(), "world bounds leave no computed band");
        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let emitted =
            process_ocean_shapefile(&missing, &world(), 0, 0, &mut sort_writer, Some(&band))
                .unwrap();
        assert_eq!(emitted, 0);
    }

    #[test]
    fn a_hash_mismatch_is_reported_before_any_record_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("mismatch.shp");
        write_test_polygon_shapefile(&shp_path);
        let mut shp = fs::read(&shp_path).unwrap();
        write_u32_le(&mut shp, 108, 3); // a malformed shape type
        fs::write(&shp_path, shp).unwrap();
        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let window = OceanPass::Low
            .emission(crate::tiling::ZoomRange::new(0, 0).unwrap())
            .unwrap();
        let wrong = OceanShapefileHashes {
            shp_xxh128: Xxh128::from_u128(1),
            shx_xxh128: Xxh128::from_u128(2),
        };
        let err = process_ocean_shapefile_checked(
            &shp_path,
            &world(),
            window,
            &mut sort_writer,
            None,
            wrong,
        )
        .expect_err("mismatch");
        assert!(
            err.to_string()
                .contains("changed after source-key resolution")
        );
    }

    #[test]
    fn processing_returns_the_hashes_of_the_bytes_it_read() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_hash_input.shp");
        write_test_polygon_shapefile(&shp_path);
        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let window = OceanPass::Low
            .emission(crate::tiling::ZoomRange::new(0, 0).unwrap())
            .unwrap();
        let (_, hashes) =
            process_ocean_shapefile_with_hashes(&shp_path, &world(), window, &mut sort_writer)
                .unwrap();
        assert_eq!(hashes.shp_xxh128, Xxh128::hash_file(&shp_path).unwrap().0);
        assert_eq!(
            hashes.shx_xxh128,
            Xxh128::hash_file(&shp_path.with_extension("shx"))
                .unwrap()
                .0
        );
    }

    #[test]
    fn a_ring_error_after_a_delivered_polygon_leaves_no_records() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("late_error.shp");
        let square = |x0: f64, y0: f64, x1: f64, y1: f64| {
            vec![(x0, y0), (x0, y1), (x1, y1), (x1, y0), (x0, y0)]
        };
        let mut unclosed = square(4.0e6, 0.0, 5.0e6, 1.0e6);
        unclosed[4] = (4.5e6, 0.5e6);
        // Reading the second outer delivers the first polygon; the third
        // ring then fails its closure check.
        write_single_record_polygon_shapefile(
            &shp_path,
            &[
                square(0.0, 0.0, 1.0e6, 1.0e6),
                square(2.0e6, 0.0, 3.0e6, 1.0e6),
                unclosed,
            ],
        );
        let chunks = dir.path().join("chunks");
        let mut sort_writer =
            SortWriter::new(&chunks, 1 << 20, sort::ChunkCompression::None).unwrap();
        let err = process_ocean_shapefile(&shp_path, &world(), 0, 0, &mut sort_writer, None)
            .expect_err("unclosed ring fails the pass");
        assert!(err.to_string().contains("not closed"));
        let mut reader = sort_writer.finish().unwrap();
        assert!(
            reader.take_partitions().is_empty(),
            "no provisional pieces escape a failed record"
        );
    }

    #[test]
    fn a_malformed_record_fails_the_pass() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("bad_part.shp");
        write_test_polygon_shapefile(&shp_path);
        let mut shp = fs::read(&shp_path).unwrap();
        write_u32_le(&mut shp, 100 + 8 + 44, 6);
        fs::write(&shp_path, shp).unwrap();
        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let err = process_ocean_shapefile(&shp_path, &world(), 0, 0, &mut sort_writer, None)
            .expect_err("out-of-range part index should fail");
        let io = std::error::Error::source(&err)
            .and_then(|e| e.downcast_ref::<std::io::Error>())
            .expect("the shapefile read error is retained as the cause");
        assert_eq!(io.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(err.kind(), crate::pipeline::ErrorKind::Failed);
    }
}
