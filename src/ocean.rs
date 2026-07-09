// Ocean shapefile processing.
//
// Reads a water-polygons-split-3857 shapefile (mmap + .shx index), filters to
// data bounds, parses polygons, and processes them in parallel with rayon.
// Uses scanline fill to minimize point-in-polygon tests.

use crate::geometry::int_ocean::{
    IntEmitScratch, IntRect, OCEAN_DP_TOL_PX, Shape, Shapes, intersect_rect_into, quantize_polygon,
    shape_bbox,
};
use crate::geometry::pyramid::{
    PyramidCell, PyramidParams, PyramidScratch, emit_shape_pyramid, emit_shape_pyramid_cell,
    split_for_parallel,
};
use crate::geometry::{self, MercBbox, Point};
use crate::mvt::GeomType;
use crate::pmtiles_writer;
use crate::shortbread::Layer;
use crate::sort::{self, SortWriter};
use crate::wire_format::{append_feature_data_with_attrs, encode_attrs_bytes};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Ocean input stats
// ---------------------------------------------------------------------------
//
// process_ocean_shapefile runs up to twice per pipeline (simplified z0-7 plus
// full-resolution z8+), so its input-side counts accumulate into these
// process-global atomics and are flushed once at OCEAN_END by
// emit_ocean_counters. Per-zoom ocean *output* is already covered by the
// sort_layer_ocean_z* firehose; the gap was the input side - how many shapefile
// shapes were read, how many overlapped the data bounds, how many polygon
// pieces they parsed into, and how many shapefile bytes were mapped. Ocean is
// ~30% of wall and a top allocator, so this is the phase most worth a look.

struct OceanStats {
    shapes: AtomicU64,
    shapes_hit: AtomicU64,
    pieces: AtomicU64,
    shapefile_bytes: AtomicU64,
}

static OCEAN_STATS: OceanStats = OceanStats {
    shapes: AtomicU64::new(0),
    shapes_hit: AtomicU64::new(0),
    pieces: AtomicU64::new(0),
    shapefile_bytes: AtomicU64::new(0),
};

/// Flush accumulated ocean input counters to the sidecar. Called once at
/// OCEAN_END; a no-op when no shapefile was processed.
pub(crate) fn emit_ocean_counters() {
    use crate::debug::emit_counter_u64;
    let shapes = OCEAN_STATS.shapes.load(Ordering::Relaxed);
    if shapes == 0 {
        return;
    }
    emit_counter_u64("ocean_shapes", shapes);
    emit_counter_u64(
        "ocean_shapes_hit",
        OCEAN_STATS.shapes_hit.load(Ordering::Relaxed),
    );
    emit_counter_u64("ocean_pieces", OCEAN_STATS.pieces.load(Ordering::Relaxed));
    emit_counter_u64(
        "ocean_shapefile_bytes",
        OCEAN_STATS.shapefile_bytes.load(Ordering::Relaxed),
    );
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

const LARGE_PIECE_VERTICES: usize = 1024;
// Cap each parallel fold accumulator's in-flight payload well below the
// global sort_budget: with (piece x zoom) fan-out across many rayon workers,
// letting each balloon to the full budget before flushing would multiply peak
// RSS by the worker count. A flush is a memcpy into the shared spill
// coalescer (which owns chunk sizing), so small thresholds are cheap - the
// old direct-to-disk flushes at this size were the main source of the NA
// chunk fragmentation (2754 chunks, fan-in 1076; see sort::SpillCoalescer).
const OCEAN_CHUNK_SIZE_LIMIT: usize = 4 * 1024 * 1024;
const RING_READ_BUFFER_BYTES: usize = 64 * 1024;

struct ParsedOceanRecord {
    pieces: Vec<Shape>,
    source_pieces: usize,
    shapes_hit: u64,
}

#[derive(Clone, Copy)]
struct ShxRecord {
    offset: usize,
    content_len: usize,
}

thread_local! {
    static OCEAN_EMIT_SCRATCH: std::cell::RefCell<PyramidScratch> =
        std::cell::RefCell::new(PyramidScratch::new());
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
    records: Vec<sort::PayloadRecord>,
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
        self.count += self.records.len() as u64;
        self.records.clear();
        self.payload.clear();
        self.bytes = 0;
    }

    fn merge_from(&mut self, other: Self) {
        self.count += other.count;

        let payload_base = self.payload.len();
        self.payload.extend(other.payload);
        self.records.extend(
            other
                .records
                .into_iter()
                .map(|(key, offset, len)| (key, payload_base + offset, len)),
        );
        self.bytes += other.bytes;
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Read ocean shapefile with mmap + bbox-first filtering, then process in parallel.
///
/// Mmaps the .shp file and reads the .shx index to get record offsets.
/// For each record, reads only the 32-byte bbox from the mmap. Only shapes
/// intersecting data_bounds get their full geometry parsed.
/// Then all polygons are processed in parallel with rayon.
#[allow(clippy::too_many_lines, clippy::unwrap_in_result)]
#[hotpath::measure]
pub(crate) fn process_ocean_shapefile(
    path: &std::path::Path,
    data_bounds: &MercBbox,
    min_zoom: u8,
    max_zoom: u8,
    sort_writer: &mut SortWriter,
) -> Result<u64, std::io::Error> {
    eprintln!("  Opening {}", path.display());

    // --- Read .shx index to get record offsets ---
    let shx_path = path.with_extension("shx");
    let shx_data = std::fs::read(&shx_path)?;

    if shx_data.len() < 100 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid .shx file {}: expected at least 100-byte header, got {} bytes",
                shx_path.display(),
                shx_data.len()
            ),
        ));
    }

    let shape_count = (shx_data.len() - 100) / 8;
    let mut records: Vec<ShxRecord> = Vec::with_capacity(shape_count);
    for i in 0..shape_count {
        let base = 100 + i * 8;
        let offset_words = i32::from_be_bytes([
            shx_data[base],
            shx_data[base + 1],
            shx_data[base + 2],
            shx_data[base + 3],
        ]);
        let content_words = i32::from_be_bytes([
            shx_data[base + 4],
            shx_data[base + 5],
            shx_data[base + 6],
            shx_data[base + 7],
        ]);
        if offset_words < 0 || content_words < 0 {
            continue;
        }
        #[allow(clippy::cast_sign_loss)]
        let offset = (offset_words as usize) * 2;
        #[allow(clippy::cast_sign_loss)]
        let content_len = (content_words as usize) * 2;
        records.push(ShxRecord {
            offset,
            content_len,
        });
    }
    eprintln!("  Index: {shape_count} shapes");

    // --- Mmap the .shp file ---
    let shp_file = std::fs::File::open(path)?;
    let shp_mmap = unsafe { memmap2::Mmap::map(&shp_file) }?;
    #[cfg(unix)]
    shp_mmap.advise(memmap2::Advice::Random)?;
    eprintln!(
        "  Mmapped {:.1} MB",
        shp_mmap.len() as f64 / (1024.0 * 1024.0)
    );
    OCEAN_STATS.shapefile_bytes.fetch_add(
        u64::try_from(shp_mmap.len()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );

    let data_rect = data_bounds_rect(data_bounds, max_zoom);

    // --- Parse phase: extract and clip polygons in parallel ---
    use rayon::prelude::*;

    let parsed_records: Vec<ParsedOceanRecord> = records
        .par_iter()
        .map(|&record| {
            parse_ocean_record(
                record,
                &shp_mmap,
                &shp_file,
                data_bounds,
                max_zoom,
                data_rect,
            )
        })
        .collect();
    #[cfg(unix)]
    unsafe {
        shp_mmap.unchecked_advise(memmap2::UncheckedAdvice::DontNeed)?;
    }
    drop(shp_mmap);
    let shapes_hit: u64 = parsed_records.iter().map(|record| record.shapes_hit).sum();
    let parsed_pieces: usize = parsed_records
        .iter()
        .map(|record| record.pieces.len())
        .sum();

    let mut pieces: Vec<Shape> = Vec::with_capacity(parsed_pieces);
    for record in parsed_records {
        pieces.extend(record.pieces);
    }

    let poly_count = pieces.len();
    eprintln!(
        "  {shape_count} shapes, {shapes_hit} in bounds, {poly_count} polygons - processing in parallel"
    );
    OCEAN_STATS.shapes.fetch_add(
        u64::try_from(shape_count).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    OCEAN_STATS
        .shapes_hit
        .fetch_add(shapes_hit, Ordering::Relaxed);
    OCEAN_STATS.pieces.fetch_add(
        u64::try_from(poly_count).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );

    // --- Process phase: parallel with rayon, direct chunk flushing ---
    //
    // Each rayon worker accumulates records in a thread-local buffer and flushes
    // directly to a chunk file when the buffer exceeds chunk_size_bytes. This
    // avoids holding all ocean sort records in memory simultaneously - at planet
    // scale that could be 10-30 GB. The previous approach (par_iter().collect()
    // into Vec<Vec<SortRecord>> + serial push) was fine for regional extracts but
    // would blow memory and serialize sort+flush at planet scale.
    use std::sync::atomic::AtomicUsize;

    let ocean_layer = Layer::Ocean as u8;
    let mut empty_attrs_bytes = Vec::new();
    encode_attrs_bytes(&mut empty_attrs_bytes, &[], max_zoom);

    // Ocean chunks use the same chunk_NNNN.bin naming (starting after PBF chunks)
    // so that --skip-to sort (SortReader::from_dir sequential scan) finds them.
    let chunk_id = std::sync::Arc::new(AtomicUsize::new(sort_writer.chunk_count()));
    let spill = sort::SpillCoalescer::new(
        sort_writer.tmp_dir().to_path_buf(),
        std::sync::Arc::clone(&chunk_id),
        sort_writer.chunk_size_bytes(),
        sort_writer.compression(),
    );
    let chunk_size = sort_writer.chunk_size_bytes().min(OCEAN_CHUNK_SIZE_LIMIT);

    let params = ocean_params(min_zoom, max_zoom);
    // Per-piece item target for the parallel frontier. Root cells of a
    // large piece usually exceed this on their own (the frontier loop is
    // then a no-op); it only forces expansion for single-root ranges.
    const SPLIT_ITEMS_PER_PIECE: usize = 16;
    // Split phase runs in parallel across pieces; each large piece emits
    // its above-frontier tiles into its own accumulator, merged (and
    // flushed) serially afterwards.
    let piece_prep: Vec<(Vec<OceanWorkItem>, Option<OceanAcc>)> = pieces
        .par_iter()
        .enumerate()
        .map(|(piece_idx, piece)| {
            let feature_id = piece_idx as u64;
            if total_vertices(piece) < LARGE_PIECE_VERTICES {
                return (
                    vec![OceanWorkItem {
                        feature_id,
                        kind: OceanWorkKind::Whole(piece_idx),
                    }],
                    None,
                );
            }
            let mut acc = OceanAcc::new();
            let cells = OCEAN_EMIT_SCRATCH.with(|cell| {
                let mut scratch = cell.borrow_mut();
                let mut sink = ocean_sink(feature_id, ocean_layer, &empty_attrs_bytes, &mut acc);
                split_for_parallel(
                    piece,
                    &params,
                    SPLIT_ITEMS_PER_PIECE,
                    &mut scratch,
                    &mut sink,
                )
            });
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
            (items, Some(acc))
        })
        .collect();

    let mut work_items = Vec::with_capacity(pieces.len());
    let mut pre_emit = OceanAcc::new();
    for (items, acc) in piece_prep {
        work_items.extend(items);
        if let Some(acc) = acc {
            pre_emit.merge_from(acc);
            if pre_emit.bytes >= chunk_size {
                pre_emit.flush(&spill);
            }
        }
    }

    let mut result = work_items
        .into_par_iter()
        .fold(OceanAcc::new, |mut acc, item| {
            emit_ocean_piece(
                item,
                &pieces,
                &params,
                ocean_layer,
                &empty_attrs_bytes,
                &mut acc,
            );
            if acc.bytes >= chunk_size {
                acc.flush(&spill);
            }
            acc
        })
        .reduce(OceanAcc::new, |mut a, b| {
            a.merge_from(b);
            if a.bytes >= chunk_size {
                a.flush(&spill);
            }
            a
        });

    result.merge_from(pre_emit);
    result.flush(&spill);
    sort_writer.adopt_chunk_files(spill.finish());
    let count = result.count;

    eprintln!("  {poly_count} polygons, {count} features");
    Ok(count)
}

fn parse_ocean_record(
    record: ShxRecord,
    shp_mmap: &memmap2::Mmap,
    shp_file: &std::fs::File,
    data_bounds: &MercBbox,
    max_zoom: u8,
    data_rect: IntRect,
) -> ParsedOceanRecord {
    let mut out = ParsedOceanRecord {
        pieces: Vec::new(),
        source_pieces: 0,
        shapes_hit: 0,
    };

    let rec = record.offset + 8;
    if record.content_len < 44 || rec + 44 > shp_mmap.len() {
        return out;
    }

    let header = {
        let shp = &shp_mmap[..];
        let mut header = [0_u8; 44];
        header.copy_from_slice(&shp[rec..rec + 44]);
        header
    };

    let xmin = f64::from_le_bytes(header[4..12].try_into().expect("shapefile field read"));
    let ymin = f64::from_le_bytes(header[12..20].try_into().expect("shapefile field read"));
    let xmax = f64::from_le_bytes(header[20..28].try_into().expect("shapefile field read"));
    let ymax = f64::from_le_bytes(header[28..36].try_into().expect("shapefile field read"));

    let merc_min = geometry::from_epsg3857(xmin, ymax);
    let merc_max = geometry::from_epsg3857(xmax, ymin);

    if merc_max.x < data_bounds.min_x
        || merc_min.x > data_bounds.max_x
        || merc_max.y < data_bounds.min_y
        || merc_min.y > data_bounds.max_y
    {
        return out;
    }

    out.shapes_hit = 1;

    // One scratch suffices: the data-rect clip and the pre-split intersect run
    // strictly sequentially (never simultaneously), and each i_overlay op clears
    // the scratch on entry. Two separate scratches doubled the per-record Overlay
    // allocation for no benefit.
    let mut scratch = IntEmitScratch::new();
    let source = ShapeRecordSource {
        record,
        rec,
        shp_len: shp_mmap.len(),
        shp_file,
        header: &header,
    };
    out.source_pieces +=
        push_shape_record_pieces(&source, &mut scratch, &mut out.pieces, max_zoom, data_rect);

    out
}

struct ShapeRecordSource<'a> {
    record: ShxRecord,
    rec: usize,
    shp_len: usize,
    shp_file: &'a std::fs::File,
    header: &'a [u8; 44],
}

fn push_shape_record_pieces(
    source: &ShapeRecordSource<'_>,
    scratch: &mut IntEmitScratch,
    pieces: &mut Vec<Shape>,
    max_zoom: u8,
    data_rect: IntRect,
) -> usize {
    let offset = source.record.offset;
    let num_parts_i32 = i32::from_le_bytes(
        source.header[36..40]
            .try_into()
            .expect("shapefile field read"),
    );
    let num_points_i32 = i32::from_le_bytes(
        source.header[40..44]
            .try_into()
            .expect("shapefile field read"),
    );
    if num_parts_i32 < 0 || num_points_i32 < 0 {
        eprintln!("  Warning: negative part/point count at offset {offset}, skipping record");
        return 0;
    }
    #[allow(clippy::cast_sign_loss)]
    let num_parts = num_parts_i32 as usize;
    #[allow(clippy::cast_sign_loss)]
    let num_points = num_points_i32 as usize;

    let parts_start = 44;
    let points_start = parts_start + num_parts * 4;
    let record_end = points_start + num_points * 16;
    if record_end > source.record.content_len || source.rec + record_end > source.shp_len {
        eprintln!("  Warning: shape record at offset {offset} extends past end of file, skipping");
        return 0;
    }

    let Some(mut ring_starts) = read_ring_starts(source, num_parts) else {
        return 0;
    };
    if ring_starts.iter().any(|&v| v > num_points) {
        eprintln!("  Warning: invalid part index at offset {offset}, skipping record");
        return 0;
    }
    ring_starts.push(num_points);

    let mut source_pieces = 0;
    let mut current_outer: Option<Vec<Point>> = None;
    let mut current_inners: Vec<Vec<Point>> = Vec::new();

    for (w, window) in ring_starts.windows(2).enumerate() {
        let Some(ring) = read_ring_points(source, points_start, window[0], window[1]) else {
            return source_pieces;
        };
        let is_outer = w == 0 || geometry::signed_area(&ring) >= 0.0;

        if is_outer {
            if let Some(outer) = current_outer.take() {
                source_pieces += push_quantized_pieces(
                    scratch,
                    pieces,
                    &outer,
                    &std::mem::take(&mut current_inners),
                    max_zoom,
                    data_rect,
                );
            }
            current_outer = Some(ring);
        } else if current_outer.is_some() {
            current_inners.push(ring);
        }
    }

    if let Some(outer) = current_outer {
        source_pieces += push_quantized_pieces(
            scratch,
            pieces,
            &outer,
            &current_inners,
            max_zoom,
            data_rect,
        );
    }

    source_pieces
}

fn read_ring_starts(source: &ShapeRecordSource<'_>, num_parts: usize) -> Option<Vec<usize>> {
    let mut parts = vec![0_u8; num_parts * 4];
    if !parts.is_empty() {
        let read_offset = u64::try_from(source.rec + 44).expect("shapefile offset fits u64");
        if let Err(err) = source.shp_file.read_exact_at(&mut parts, read_offset) {
            eprintln!(
                "  Warning: failed to read shape parts at offset {}: {err}",
                source.record.offset
            );
            return None;
        }
    }
    Some(
        (0..num_parts)
            .map(|j| {
                let b = j * 4;
                let v =
                    i32::from_le_bytes(parts[b..b + 4].try_into().expect("shapefile field read"));
                if v < 0 {
                    usize::MAX
                } else {
                    #[allow(clippy::cast_sign_loss)]
                    {
                        v as usize
                    }
                }
            })
            .collect(),
    )
}

fn read_ring_points(
    source: &ShapeRecordSource<'_>,
    points_start: usize,
    start: usize,
    end: usize,
) -> Option<Vec<Point>> {
    let point_count = end - start;
    let mut ring = Vec::with_capacity(point_count);
    let mut buf = vec![0_u8; RING_READ_BUFFER_BYTES];
    let mut points_read = 0usize;

    while points_read < point_count {
        let points_this_read = ((point_count - points_read) * 16).min(buf.len()) / 16;
        let byte_len = points_this_read * 16;
        let read_offset = u64::try_from(source.rec + points_start + (start + points_read) * 16)
            .expect("shapefile offset fits u64");
        if let Err(err) = source
            .shp_file
            .read_exact_at(&mut buf[..byte_len], read_offset)
        {
            eprintln!(
                "  Warning: failed to read shape points at offset {}: {err}",
                source.record.offset
            );
            return None;
        }
        for idx in 0..points_this_read {
            let b = idx * 16;
            let x = f64::from_le_bytes(buf[b..b + 8].try_into().expect("shapefile field read"));
            let y =
                f64::from_le_bytes(buf[b + 8..b + 16].try_into().expect("shapefile field read"));
            ring.push(geometry::from_epsg3857(x, y));
        }
        points_read += points_this_read;
    }
    Some(ring)
}

fn push_quantized_pieces(
    scratch: &mut IntEmitScratch,
    pieces: &mut Vec<Shape>,
    outer: &[Point],
    inners: &[Vec<Point>],
    max_zoom: u8,
    data_rect: IntRect,
) -> usize {
    let shape = quantize_polygon(outer, inners, max_zoom);
    if shape.is_empty() {
        return 0;
    }

    // The common case for an in-bounds extract: the shape lies entirely
    // inside the data bounds - the boolean is an expensive identity.
    if shape_bbox(&shape).is_some_and(|bb| rect_contains(data_rect, bb)) {
        pieces.push(shape);
        return 1;
    }

    let mut clipped = Vec::new();
    intersect_rect_into(scratch, &shape, data_rect, 0, &mut clipped);
    let source_pieces = clipped.len();
    for piece in clipped {
        pieces.push(piece);
    }
    source_pieces
}

/// True if `outer` contains `inner` (closed containment).
fn rect_contains(outer: IntRect, inner: IntRect) -> bool {
    outer.min_x <= inner.min_x
        && outer.min_y <= inner.min_y
        && outer.max_x >= inner.max_x
        && outer.max_y >= inner.max_y
}

fn data_bounds_rect(data_bounds: &MercBbox, max_zoom: u8) -> IntRect {
    let scale = 1_i64 << (u32::from(max_zoom) + 12);
    IntRect {
        min_x: merc_floor(data_bounds.min_x, scale),
        min_y: merc_floor(data_bounds.min_y, scale),
        max_x: merc_ceil(data_bounds.max_x, scale),
        max_y: merc_ceil(data_bounds.max_y, scale),
    }
}

fn merc_floor(v: f64, scale: i64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let q = (v * scale as f64).floor() as i64;
    i32::try_from(q.clamp(0, scale)).expect("base ocean coordinate fits i32")
}

fn merc_ceil(v: f64, scale: i64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let q = (v * scale as f64).ceil() as i64;
    i32::try_from(q.clamp(0, scale)).expect("base ocean coordinate fits i32")
}

fn total_vertices(piece: &Shape) -> usize {
    piece.iter().map(Vec::len).sum()
}

fn ocean_params(min_zoom: u8, max_zoom: u8) -> PyramidParams<'static> {
    PyramidParams {
        maxz: max_zoom,
        z_top: min_zoom,
        z_bottom: max_zoom,
        dp_tol: &ocean_dp_tol,
        min_area: &ocean_min_area,
        pins: None,
    }
}

fn ocean_dp_tol(_z: u8) -> i64 {
    OCEAN_DP_TOL_PX
}

fn ocean_min_area(_z: u8) -> u64 {
    256
}

fn ocean_sink<'a>(
    feature_id: u64,
    layer_idx: u8,
    attrs_bytes: &'a [u8],
    acc: &'a mut OceanAcc,
) -> impl FnMut(u8, u32, u32, &[u32]) + 'a {
    move |z: u8, tx: u32, ty: u32, geom: &[u32]| {
        let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
        let key = sort::make_sort_key(tile_id, layer_idx, 0);
        let range = append_feature_data_with_attrs(
            &mut acc.payload,
            feature_id,
            GeomType::Polygon,
            geom,
            attrs_bytes,
        );
        acc.bytes += range.len() + std::mem::size_of::<sort::PayloadRecord>();
        acc.records.push((key, range.start, range.len()));
    }
}

#[hotpath::measure]
fn emit_ocean_piece(
    item: OceanWorkItem,
    pieces: &[Shape],
    params: &PyramidParams<'_>,
    layer_idx: u8,
    attrs_bytes: &[u8],
    acc: &mut OceanAcc,
) {
    OCEAN_EMIT_SCRATCH.with(|cell| {
        let mut scratch = cell.borrow_mut();
        let mut sink = ocean_sink(item.feature_id, layer_idx, attrs_bytes, acc);
        match item.kind {
            OceanWorkKind::Whole(piece_idx) => {
                emit_shape_pyramid(&pieces[piece_idx], params, &mut scratch, &mut sink);
            }
            OceanWorkKind::Cell(cell, frag) => {
                emit_shape_pyramid_cell(cell, frag, params, &mut scratch, &mut sink);
            }
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::geometry::Point;
    use crate::sort::SortWriter;
    use std::fs;
    use std::path::Path;

    fn write_u32_be(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
    }

    fn write_u32_le(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn write_f64_le(buf: &mut [u8], off: usize, v: f64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn write_single_record_polygon_shapefile(path: &Path, parts: &[Vec<(f64, f64)>]) {
        let num_parts = parts.len();
        let num_points: usize = parts.iter().map(std::vec::Vec::len).sum();
        assert!(num_parts > 0);
        assert!(num_points > 0);

        let mut xmin = f64::INFINITY;
        let mut ymin = f64::INFINITY;
        let mut xmax = f64::NEG_INFINITY;
        let mut ymax = f64::NEG_INFINITY;
        for ring in parts {
            for &(x, y) in ring {
                xmin = xmin.min(x);
                ymin = ymin.min(y);
                xmax = xmax.max(x);
                ymax = ymax.max(y);
            }
        }

        // Record content length (bytes):
        // shape_type(4) + bbox(32) + num_parts(4) + num_points(4) + parts(num_parts*4) + points(num_points*16)
        let record_content_bytes = 4 + 32 + 4 + 4 + num_parts * 4 + num_points * 16;
        let record_content_words =
            u32::try_from(record_content_bytes / 2).expect("record content length fits u32");
        let shp_file_len_words =
            u32::try_from((100 + 8 + record_content_bytes) / 2).expect("shp length fits u32");
        let shx_file_len_words = ((100 + 8) / 2) as u32;

        let mut shp = vec![0u8; 100 + 8 + record_content_bytes];
        let mut shx = vec![0u8; 108];

        // Common 100-byte file header (shp + shx)
        // Big-endian section
        write_u32_be(&mut shp, 0, 9994);
        write_u32_be(&mut shp, 24, shp_file_len_words);
        write_u32_be(&mut shx, 0, 9994);
        write_u32_be(&mut shx, 24, shx_file_len_words);
        // Little-endian section
        write_u32_le(&mut shp, 28, 1000); // version
        write_u32_le(&mut shp, 32, 5); // Polygon
        write_u32_le(&mut shx, 28, 1000);
        write_u32_le(&mut shx, 32, 5);
        // Header bbox
        write_f64_le(&mut shp, 36, xmin);
        write_f64_le(&mut shp, 44, ymin);
        write_f64_le(&mut shp, 52, xmax);
        write_f64_le(&mut shp, 60, ymax);
        write_f64_le(&mut shx, 36, xmin);
        write_f64_le(&mut shx, 44, ymin);
        write_f64_le(&mut shx, 52, xmax);
        write_f64_le(&mut shx, 60, ymax);

        // shx index record
        write_u32_be(&mut shx, 100, 50); // .shp record offset in 16-bit words (100 bytes)
        write_u32_be(&mut shx, 104, record_content_words);

        // shp record header
        let rec_header = 100;
        write_u32_be(&mut shp, rec_header, 1); // record number
        write_u32_be(&mut shp, rec_header + 4, record_content_words);

        // shp record content
        let rec = rec_header + 8;
        write_u32_le(&mut shp, rec, 5); // Polygon
        write_f64_le(&mut shp, rec + 4, xmin); // xmin
        write_f64_le(&mut shp, rec + 12, ymin); // ymin
        write_f64_le(&mut shp, rec + 20, xmax); // xmax
        write_f64_le(&mut shp, rec + 28, ymax); // ymax
        write_u32_le(
            &mut shp,
            rec + 36,
            u32::try_from(num_parts).expect("part count fits u32"),
        ); // num_parts
        write_u32_le(
            &mut shp,
            rec + 40,
            u32::try_from(num_points).expect("point count fits u32"),
        ); // num_points
        let mut part_start = 0usize;
        for (i, ring) in parts.iter().enumerate() {
            write_u32_le(
                &mut shp,
                rec + 44 + i * 4,
                u32::try_from(part_start).expect("part start fits u32"),
            );
            part_start += ring.len();
        }

        let mut p = rec + 44 + num_parts * 4;
        for ring in parts {
            for &(x, y) in ring {
                write_f64_le(&mut shp, p, x);
                write_f64_le(&mut shp, p + 8, y);
                p += 16;
            }
        }

        fs::write(path, shp).unwrap();
        fs::write(path.with_extension("shx"), shx).unwrap();
    }

    fn write_test_polygon_shapefile(path: &Path) {
        // One world-sized square in EPSG:3857 meters.
        let half_c = 20_037_508.343;
        let outer = vec![
            (-half_c, -half_c),
            (half_c, -half_c),
            (half_c, half_c),
            (-half_c, half_c),
            (-half_c, -half_c),
        ];
        write_single_record_polygon_shapefile(path, &[outer]);
    }

    fn write_test_polygon_with_hole_shapefile(path: &Path) {
        let half_c = 20_037_508.343;
        let outer = vec![
            (-half_c, -half_c),
            (half_c, -half_c),
            (half_c, half_c),
            (-half_c, half_c),
            (-half_c, -half_c),
        ];
        // Clockwise inner ring so parser classifies it as a hole.
        let hole = vec![
            (-half_c * 0.5, -half_c * 0.5),
            (-half_c * 0.5, half_c * 0.5),
            (half_c * 0.5, half_c * 0.5),
            (half_c * 0.5, -half_c * 0.5),
            (-half_c * 0.5, -half_c * 0.5),
        ];
        write_single_record_polygon_shapefile(path, &[outer, hole]);
    }

    // -----------------------------------------------------------------------
    // point_in_polygon tests
    // -----------------------------------------------------------------------

    fn unit_square() -> Vec<Point> {
        vec![
            Point { x: 0.0, y: 0.0 },
            Point { x: 1.0, y: 0.0 },
            Point { x: 1.0, y: 1.0 },
            Point { x: 0.0, y: 1.0 },
        ]
    }

    #[test]
    fn pip_inside_square() {
        assert!(geometry::point_in_polygon(
            &Point::new(0.5, 0.5),
            &unit_square()
        ));
    }

    #[test]
    fn pip_outside_square() {
        assert!(!geometry::point_in_polygon(
            &Point::new(2.0, 0.5),
            &unit_square()
        ));
    }

    #[test]
    fn pip_on_edge() {
        // Edge behavior is implementation-defined for ray-casting;
        // just verify it does not panic.
        let _ = geometry::point_in_polygon(&Point::new(0.5, 0.0), &unit_square());
    }

    #[test]
    fn pip_inside_triangle() {
        let tri = vec![
            Point { x: 0.0, y: 0.0 },
            Point { x: 4.0, y: 0.0 },
            Point { x: 2.0, y: 3.0 },
        ];
        assert!(geometry::point_in_polygon(&Point::new(2.0, 1.0), &tri));
    }

    #[test]
    fn pip_outside_triangle() {
        let tri = vec![
            Point { x: 0.0, y: 0.0 },
            Point { x: 4.0, y: 0.0 },
            Point { x: 2.0, y: 3.0 },
        ];
        assert!(!geometry::point_in_polygon(&Point::new(0.0, 3.0), &tri));
    }

    #[test]
    fn pip_degenerate() {
        // Fewer than 3 points should return false
        assert!(!geometry::point_in_polygon(&Point::new(0.0, 0.0), &[]));
        assert!(!geometry::point_in_polygon(
            &Point::new(0.0, 0.0),
            &[Point { x: 0.0, y: 0.0 }]
        ));
        assert!(!geometry::point_in_polygon(
            &Point::new(0.0, 0.0),
            &[Point { x: 0.0, y: 0.0 }, Point { x: 1.0, y: 1.0 }],
        ));
    }

    #[test]
    fn pip_concave() {
        // An L-shaped (concave) polygon:
        //
        //   (0,0)---(2,0)
        //     |        |
        //   (0,2)---(1,2)
        //            |
        //   (0,3)---(1,3)   <-- not connected; full shape below
        //
        // Vertices (counter-clockwise on a math-y axis):
        //   (0,0) (2,0) (2,1) (1,1) (1,3) (0,3)
        let l_shape = vec![
            Point { x: 0.0, y: 0.0 },
            Point { x: 2.0, y: 0.0 },
            Point { x: 2.0, y: 1.0 },
            Point { x: 1.0, y: 1.0 },
            Point { x: 1.0, y: 3.0 },
            Point { x: 0.0, y: 3.0 },
        ];

        // Inside the body of the L
        assert!(geometry::point_in_polygon(&Point::new(0.5, 0.5), &l_shape));
        assert!(geometry::point_in_polygon(&Point::new(0.5, 2.0), &l_shape));

        // Inside the concavity (the cut-out region) - should be false
        assert!(!geometry::point_in_polygon(&Point::new(1.5, 2.0), &l_shape));
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

        let emitted = process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer).unwrap();

        assert!(emitted > 0, "expected ocean features to be emitted");

        let mut reader = sort_writer.finish().unwrap();
        let mut count = 0u64;
        while reader.next().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, emitted, "emitted count should match sorted records");
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
            process_ocean_shapefile(&shp_path, &disjoint_bounds, 0, 0, &mut sort_writer).unwrap();

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

        let emitted = process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer).unwrap();
        assert!(emitted > 0, "expected ocean features to be emitted");

        let mut reader = sort_writer.finish().unwrap();
        let rec = reader
            .next()
            .unwrap()
            .expect("expected at least one record");
        let cmd_count = u16::from_le_bytes(rec.data[9..11].try_into().unwrap());
        assert!(
            cmd_count >= 6,
            "polygon with hole should encode multiple rings (cmd_count={cmd_count})"
        );
    }

    #[test]
    fn process_ocean_shapefile_rejects_short_shx_header() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_bad_shx.shp");
        write_test_polygon_shapefile(&shp_path);
        fs::write(shp_path.with_extension("shx"), [0u8; 64]).unwrap();

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let err = process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer)
            .expect_err("short .shx header should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("invalid .shx file"));
    }

    #[test]
    fn process_ocean_shapefile_errors_when_shx_missing() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_missing_shx.shp");
        write_test_polygon_shapefile(&shp_path);
        fs::remove_file(shp_path.with_extension("shx")).unwrap();

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let err = process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer)
            .expect_err("missing .shx should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn process_ocean_shapefile_skips_truncated_shp_record() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_truncated_shp.shp");
        write_test_polygon_shapefile(&shp_path);

        let mut shp_data = fs::read(&shp_path).unwrap();
        shp_data.truncate(120);
        fs::write(&shp_path, shp_data).unwrap();

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let emitted = process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer).unwrap();
        assert_eq!(emitted, 0, "truncated record should be skipped");
    }
}
