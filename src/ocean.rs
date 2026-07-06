// Ocean shapefile processing.
//
// Reads a water-polygons-split-3857 shapefile (mmap + .shx index), filters to
// data bounds, parses polygons, and processes them in parallel with rayon.
// Uses scanline fill to minimize point-in-polygon tests.

use crate::geometry::{self, MercBbox, Point, BUFFER_FRACTION, close_and_orient_cw, close_and_orient_ccw};
use crate::geometry::int_ocean::{
    IntRect, Shape, Shapes, OCEAN_DP_TOL_PX,
    intersect_rect, normalize, point_in_shape, quantize_polygon, rescale_shape, simplify_shape_dp,
};
use crate::mvt::{self, GeomType};
use crate::pmtiles_writer;
use crate::shortbread::{self, Layer};
use crate::sort::{self, SortRecord, SortWriter};
use crate::wire_format::encode_feature_data;

use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

const TILE_EXTENT_I32: i32 = 4096;
const TILE_BUFFER_I32: i32 = 128;

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
            format!("invalid .shx file {}: expected at least 100-byte header, got {} bytes",
                shx_path.display(), shx_data.len()),
        ));
    }

    let shape_count = (shx_data.len() - 100) / 8;
    let mut offsets: Vec<usize> = Vec::with_capacity(shape_count);
    for i in 0..shape_count {
        let base = 100 + i * 8;
        let offset_words = i32::from_be_bytes([
            shx_data[base], shx_data[base + 1], shx_data[base + 2], shx_data[base + 3],
        ]);
        if offset_words < 0 {
            continue;
        }
        #[allow(clippy::cast_sign_loss)]
        offsets.push((offset_words as usize) * 2);
    }
    eprintln!("  Index: {shape_count} shapes");

    // --- Mmap the .shp file ---
    let shp_file = std::fs::File::open(path)?;
    let shp_mmap = unsafe { memmap2::Mmap::map(&shp_file) }?;
    let shp = &shp_mmap[..];
    eprintln!("  Mmapped {:.1} MB", shp.len() as f64 / (1024.0 * 1024.0));

    let data_rect = data_bounds_rect(data_bounds, max_zoom);

    // --- Parse phase: extract all polygons (single-threaded, sequential I/O) ---
    let mut pieces: Vec<Shape> = Vec::new();
    let mut shapes_hit: u64 = 0;

    for &offset in &offsets {
        let rec = offset + 8;
        if rec + 44 > shp.len() {
            break;
        }

        let bb = rec + 4;
        let xmin = f64::from_le_bytes(shp[bb..bb + 8].try_into().expect("shapefile field read"));
        let ymin = f64::from_le_bytes(shp[bb + 8..bb + 16].try_into().expect("shapefile field read"));
        let xmax = f64::from_le_bytes(shp[bb + 16..bb + 24].try_into().expect("shapefile field read"));
        let ymax = f64::from_le_bytes(shp[bb + 24..bb + 32].try_into().expect("shapefile field read"));

        let merc_min = geometry::from_epsg3857(xmin, ymax);
        let merc_max = geometry::from_epsg3857(xmax, ymin);

        if merc_max.x < data_bounds.min_x
            || merc_min.x > data_bounds.max_x
            || merc_max.y < data_bounds.min_y
            || merc_min.y > data_bounds.max_y
        {
            continue;
        }

        shapes_hit += 1;

        let num_parts_i32 = i32::from_le_bytes(shp[rec + 36..rec + 40].try_into().expect("shapefile field read"));
        let num_points_i32 = i32::from_le_bytes(shp[rec + 40..rec + 44].try_into().expect("shapefile field read"));
        if num_parts_i32 < 0 || num_points_i32 < 0 {
            eprintln!("  Warning: negative part/point count at offset {offset}, skipping record");
            continue;
        }
        #[allow(clippy::cast_sign_loss)]
        let num_parts = num_parts_i32 as usize;
        #[allow(clippy::cast_sign_loss)]
        let num_points = num_points_i32 as usize;

        let parts_start = rec + 44;
        let points_start = parts_start + num_parts * 4;
        let record_end = points_start + num_points * 16;
        if record_end > shp.len() {
            eprintln!("  Warning: shape record at offset {offset} extends past end of file, skipping");
            continue;
        }

        let mut ring_starts: Vec<usize> = (0..num_parts)
            .map(|j| {
                let b = parts_start + j * 4;
                let v = i32::from_le_bytes(shp[b..b + 4].try_into().expect("shapefile field read"));
                if v < 0 { usize::MAX } else {
                    #[allow(clippy::cast_sign_loss)]
                    { v as usize }
                }
            })
            .collect();
        if ring_starts.iter().any(|&v| v > num_points) {
            eprintln!("  Warning: invalid part index at offset {offset}, skipping record");
            continue;
        }
        ring_starts.push(num_points);

        let all_points: Vec<Point> = (0..num_points)
            .map(|j| {
                let b = points_start + j * 16;
                let x = f64::from_le_bytes(shp[b..b + 8].try_into().expect("shapefile field read"));
                let y = f64::from_le_bytes(shp[b + 8..b + 16].try_into().expect("shapefile field read"));
                geometry::from_epsg3857(x, y)
            })
            .collect();

        let mut current_outer: Option<Vec<Point>> = None;
        let mut current_inners: Vec<Vec<Point>> = Vec::new();

        for (w, window) in ring_starts.windows(2).enumerate() {
            let ring = &all_points[window[0]..window[1]];

            let is_outer = w == 0 || geometry::signed_area(ring) >= 0.0;

            if is_outer {
                if let Some(outer) = current_outer.take() {
                    push_quantized_pieces(
                        &mut pieces,
                        &outer,
                        &std::mem::take(&mut current_inners),
                        max_zoom,
                        data_rect,
                    );
                }
                current_outer = Some(ring.to_vec());
            } else if current_outer.is_some() {
                current_inners.push(ring.to_vec());
            }
        }

        if let Some(outer) = current_outer {
            push_quantized_pieces(
                &mut pieces,
                &outer,
                &current_inners,
                max_zoom,
                data_rect,
            );
        }
    }

    // Pre-split large polygons along grid-aligned tile boundaries to reduce
    // per-polygon vertex count. Splitting at zoom SPLIT_Z means each sub-polygon
    // fits within one tile at SPLIT_Z, spanning at most 2^(z-SPLIT_Z)² tiles at
    // finer zooms. This dramatically reduces DP simplification cost (O(n log n))
    // and per-row clip input size. Only split polygons above a vertex threshold.
    const SPLIT_Z: u8 = 8;
    const SPLIT_MIN_VERTICES: usize = 500;
    if max_zoom >= SPLIT_Z {
        let orig_count = pieces.len();
        let mut split_out: Vec<Shape> = Vec::with_capacity(pieces.len());
        let split_tile_size = 1_i32 << (u32::from(max_zoom - SPLIT_Z) + 12);
        let max_split_tile = i32::from((1_u16 << SPLIT_Z) - 1);
        for piece in pieces.drain(..) {
            if piece.first().map_or(0, Vec::len) < SPLIT_MIN_VERTICES {
                split_out.push(piece);
                continue;
            }
            let Some(bb) = shape_bbox(&piece) else {
                continue;
            };
            let stx_min = (bb.min_x.div_euclid(split_tile_size)).clamp(0, max_split_tile);
            let sty_min = (bb.min_y.div_euclid(split_tile_size)).clamp(0, max_split_tile);
            let stx_max = (bb.max_x.div_euclid(split_tile_size)).clamp(0, max_split_tile);
            let sty_max = (bb.max_y.div_euclid(split_tile_size)).clamp(0, max_split_tile);
            if stx_min == stx_max && sty_min == sty_max {
                split_out.push(piece);
                continue;
            }
            for sty in sty_min..=sty_max {
                for stx in stx_min..=stx_max {
                    let tile_rect = IntRect {
                        min_x: stx * split_tile_size,
                        min_y: sty * split_tile_size,
                        max_x: (stx + 1) * split_tile_size,
                        max_y: (sty + 1) * split_tile_size,
                    };
                    split_out.extend(intersect_rect(&piece, tile_rect, 0));
                }
            }
        }
        pieces = split_out;
        if pieces.len() != orig_count {
            eprintln!("  Pre-split at z{SPLIT_Z}: {orig_count} -> {} polygons", pieces.len());
        }
    }

    let poly_count = pieces.len();
    eprintln!("  {shape_count} shapes, {shapes_hit} in bounds, {poly_count} polygons - processing in parallel");

    // --- Process phase: parallel with rayon, direct chunk flushing ---
    //
    // Each rayon worker accumulates records in a thread-local buffer and flushes
    // directly to a chunk file when the buffer exceeds chunk_size_bytes. This
    // avoids holding all ocean sort records in memory simultaneously - at planet
    // scale that could be 10-30 GB. The previous approach (par_iter().collect()
    // into Vec<Vec<SortRecord>> + serial push) was fine for regional extracts but
    // would blow memory and serialize sort+flush at planet scale.
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let ocean_layer = Layer::Ocean as u8;
    let empty_attrs: Vec<shortbread::Attr> = Vec::new();

    // Ocean chunks use the same chunk_NNNN.bin naming (starting after PBF chunks)
    // so that --skip-to sort (SortReader::from_dir sequential scan) finds them.
    let chunk_id = AtomicUsize::new(sort_writer.chunk_count());
    let chunk_dir = sort_writer.tmp_dir().to_path_buf();
    let chunk_size = sort_writer.chunk_size_bytes();
    let chunk_compression = sort_writer.compression();

    struct OceanAcc {
        records: Vec<SortRecord>,
        bytes: usize,
        chunk_paths: Vec<std::path::PathBuf>,
        count: u64,
        compression: sort::ChunkCompression,
    }

    impl OceanAcc {
        fn flush(&mut self, chunk_dir: &std::path::Path, chunk_id: &AtomicUsize) {
            if self.records.is_empty() {
                return;
            }
            let id = chunk_id.fetch_add(1, Ordering::Relaxed);
            let path = chunk_dir.join(format!("chunk_{id:04}.bin"));
            // Panic: inside rayon fold - can't propagate Result. Disk I/O failure is unrecoverable.
            sort::write_sorted_chunk(&mut self.records, &path, self.compression)
                .expect("ocean chunk write failed");
            self.chunk_paths.push(path);
            self.count += self.records.len() as u64;
            self.records.clear();
            self.bytes = 0;
        }
    }

    let result = pieces
        .par_iter()
        .enumerate()
        .fold(
            || OceanAcc { records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0, compression: chunk_compression },
            |mut acc, (idx, piece)| {
                let before = acc.records.len();
                emit_ocean_polygon(
                    idx as u64, piece,
                    min_zoom, max_zoom, ocean_layer, &empty_attrs,
                    &mut acc.records,
                );
                for r in &acc.records[before..] {
                    acc.bytes += r.data.len() + std::mem::size_of::<sort::SortRecord>();
                }
                if acc.bytes >= chunk_size {
                    acc.flush(&chunk_dir, &chunk_id);
                }
                acc
            },
        )
        .map(|mut acc| {
            acc.flush(&chunk_dir, &chunk_id);
            acc
        })
        .reduce(
            || OceanAcc { records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0, compression: chunk_compression },
            |mut a, b| {
                a.chunk_paths.extend(b.chunk_paths);
                a.count += b.count;
                a
            },
        );

    sort_writer.adopt_chunk_files(result.chunk_paths);
    let count = result.count;

    eprintln!("  {poly_count} polygons, {count} features");
    Ok(count)
}

fn push_quantized_pieces(
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
    pieces.extend(intersect_rect(&shape, data_rect, 0));
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

fn shape_bbox(shape: &Shape) -> Option<IntRect> {
    let mut points = shape.iter().flatten();
    let first = points.next()?;
    let (mut min_x, mut max_x) = (first.x, first.x);
    let (mut min_y, mut max_y) = (first.y, first.y);
    for p in points {
        min_x = min_x.min(p.x);
        max_x = max_x.max(p.x);
        min_y = min_y.min(p.y);
        max_y = max_y.max(p.y);
    }
    Some(IntRect { min_x, min_y, max_x, max_y })
}

fn tile_range_for_rect(rect: IntRect, max_tile: u32) -> (u32, u32, u32, u32) {
    (
        tile_index(rect.min_x, max_tile),
        tile_index(rect.max_x, max_tile),
        tile_index(rect.min_y, max_tile),
        tile_index(rect.max_y, max_tile),
    )
}

fn tile_index(q: i32, max_tile: u32) -> u32 {
    if q <= 0 {
        0
    } else {
        #[allow(clippy::cast_sign_loss)]
        let idx = (q / TILE_EXTENT_I32) as u32;
        idx.min(max_tile)
    }
}

fn row_band_rect(ty: u32, world_max: i32) -> IntRect {
    IntRect {
        min_x: 0,
        min_y: tile_origin(ty) - TILE_BUFFER_I32,
        max_x: world_max,
        max_y: tile_origin(ty + 1) + TILE_BUFFER_I32,
    }
}

fn tile_origin(t: u32) -> i32 {
    i32::try_from(t).expect("tile coordinate fits i32") * TILE_EXTENT_I32
}

fn tile_center_coord(t: u32) -> i32 {
    tile_origin(t) + TILE_EXTENT_I32 / 2
}

fn buffered_tile_rect(tx: u32, ty: u32) -> IntRect {
    IntRect {
        min_x: tile_origin(tx) - TILE_BUFFER_I32,
        min_y: tile_origin(ty) - TILE_BUFFER_I32,
        max_x: tile_origin(tx + 1) + TILE_BUFFER_I32,
        max_y: tile_origin(ty + 1) + TILE_BUFFER_I32,
    }
}

fn buffered_tile_rect_contained(tx: u32, ty: u32, rect: IntRect) -> bool {
    let tile = buffered_tile_rect(tx, ty);
    tile.min_x >= rect.min_x
        && tile.max_x <= rect.max_x
        && tile.min_y >= rect.min_y
        && tile.max_y <= rect.max_y
}

fn fast_path_rect(row_shapes: &Shapes, shape_bbox: IntRect, band: IntRect) -> Option<IntRect> {
    if row_shapes.len() != 1 || row_shapes[0].len() != 1 || row_shapes[0][0].len() != 4 {
        return None;
    }

    let rect = rect_intersection(shape_bbox, band)?;
    let mut actual = row_shapes[0][0]
        .iter()
        .map(|p| (p.x, p.y))
        .collect::<Vec<_>>();
    actual.sort_unstable();
    let mut expected = vec![
        (rect.min_x, rect.min_y),
        (rect.max_x, rect.min_y),
        (rect.max_x, rect.max_y),
        (rect.min_x, rect.max_y),
    ];
    expected.sort_unstable();
    (actual == expected).then_some(rect)
}

fn rect_intersection(a: IntRect, b: IntRect) -> Option<IntRect> {
    let rect = IntRect {
        min_x: a.min_x.max(b.min_x),
        min_y: a.min_y.max(b.min_y),
        max_x: a.max_x.min(b.max_x),
        max_y: a.max_y.min(b.max_y),
    };
    (rect.min_x < rect.max_x && rect.min_y < rect.max_y).then_some(rect)
}

fn debug_assert_no_boundary_in_fast_tiles(
    boundary_tiles: &HashSet<u64>,
    ty: u32,
    tx_min: u32,
    tx_max: u32,
    rect: IntRect,
) {
    #[cfg(debug_assertions)]
    for tx in tx_min..=tx_max {
        if buffered_tile_rect_contained(tx, ty, rect) {
            debug_assert!(!boundary_tiles.contains(&pack_tile(tx, ty)));
        }
    }
}

// ---------------------------------------------------------------------------
// Ocean polygon emission (scanline fill)
// ---------------------------------------------------------------------------

/// Emit an ocean polygon with scanline fill optimization.
///
/// For each zoom level:
/// 1. Rasterize polygon edges → boundary tiles (DDA grid traversal)
/// 2. Group boundary tiles by row, sort by x
/// 3. For each row: clip+encode boundary tiles, then for each gap between
///    boundary tiles do ONE point-in-polygon test and fill the entire gap.
/// 4. Rows with no boundary tiles: single PIP test, fill entire row if inside.
///
/// Reduces PIP calls from O(bbox_tiles) to O(gaps × rows).
#[hotpath::measure]
#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::cognitive_complexity)]
fn emit_ocean_polygon(
    feature_id: u64,
    piece: &Shape,
    min_zoom: u8,
    max_zoom: u8,
    layer_idx: u8,
    attrs: &[shortbread::Attr],
    records: &mut Vec<SortRecord>,
) {
    if piece.is_empty() {
        return;
    }

    let mut boundary_tiles: HashSet<u64> = HashSet::new();
    let mut boundary_rows: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut bt_all_rings: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut bt_geom_buf: Vec<u32> = Vec::new();

    for z in (min_zoom..=max_zoom).rev() {
        let max_tile = (1u32 << z) - 1;
        let world_max = i32::try_from((u64::from(max_tile) + 1) * u64::from(TILE_EXTENT_I32 as u32))
            .expect("z14 world extent fits i32");

        let mut shape_z = rescale_shape(piece, max_zoom - z);
        simplify_shape_dp(&mut shape_z, OCEAN_DP_TOL_PX);
        for shape in normalize(shape_z, 256) {
            let Some(bbox) = shape_bbox(&shape) else {
                continue;
            };
            let (tx_min, tx_max, ty_min, ty_max) = tile_range_for_rect(bbox, max_tile);

            boundary_tiles.clear();
            rasterize_shape_edges(&shape, max_tile, &mut boundary_tiles);
            boundary_rows.clear();
            for &packed in &boundary_tiles {
                #[allow(clippy::cast_possible_truncation)]
                let tx = (packed >> 32) as u32;
                #[allow(clippy::cast_possible_truncation)]
                let ty = packed as u32;
                boundary_rows.entry(ty).or_default().push(tx);
            }
            for txs in boundary_rows.values_mut() {
                txs.sort_unstable();
                txs.dedup();
            }

            for ty in ty_min..=ty_max {
                let band = row_band_rect(ty, world_max);
                let row_shapes = intersect_rect(&shape, band, 256);
                if row_shapes.is_empty() {
                    continue;
                }

                if let Some(r) = fast_path_rect(&row_shapes, bbox, band) {
                    debug_assert_no_boundary_in_fast_tiles(&boundary_tiles, ty, tx_min, tx_max, r);
                    for tx in tx_min..=tx_max {
                        if buffered_tile_rect_contained(tx, ty, r) {
                            emit_full_tile(
                                feature_id,
                                tx,
                                ty,
                                z,
                                layer_idx,
                                attrs,
                                records,
                                &mut bt_all_rings,
                                &mut bt_geom_buf,
                            );
                        } else {
                            emit_tile_for_row_shapes(
                                feature_id,
                                tx,
                                ty,
                                z,
                                &row_shapes,
                                layer_idx,
                                attrs,
                                records,
                                &mut bt_all_rings,
                                &mut bt_geom_buf,
                            );
                        }
                    }
                    continue;
                }

                let boundary_txs = boundary_rows.get(&ty).map_or(&[][..], Vec::as_slice);
                for row_shape in &row_shapes {
                    let Some(row_bbox) = shape_bbox(row_shape) else {
                        continue;
                    };
                    let (row_tx_min, row_tx_max, _, _) = tile_range_for_rect(row_bbox, max_tile);
                    emit_boundary_and_gap_tiles(
                        feature_id,
                        ty,
                        z,
                        row_tx_min,
                        row_tx_max,
                        boundary_txs,
                        row_shape,
                        layer_idx,
                        attrs,
                        records,
                        &mut bt_all_rings,
                        &mut bt_geom_buf,
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_boundary_and_gap_tiles(
    feature_id: u64,
    ty: u32,
    z: u8,
    tx_min: u32,
    tx_max: u32,
    boundary_txs: &[u32],
    row_shape: &Shape,
    layer_idx: u8,
    attrs: &[shortbread::Attr],
    records: &mut Vec<SortRecord>,
    all_rings: &mut Vec<Vec<(i32, i32)>>,
    geom_buf: &mut Vec<u32>,
) {
    if tx_min > tx_max {
        return;
    }

    let mut cursor = tx_min;
    for &boundary_tx in boundary_txs {
        if boundary_tx < tx_min || boundary_tx > tx_max {
            continue;
        }
        if cursor < boundary_tx {
            emit_gap_run(
                feature_id,
                cursor,
                boundary_tx - 1,
                ty,
                z,
                row_shape,
                layer_idx,
                attrs,
                records,
                all_rings,
                geom_buf,
            );
        }
        emit_clipped_tile_shape(
            feature_id, boundary_tx, ty, z, row_shape, layer_idx, attrs, records, all_rings, geom_buf,
        );
        cursor = boundary_tx.saturating_add(1);
    }

    if cursor <= tx_max {
        emit_gap_run(
            feature_id,
            cursor,
            tx_max,
            ty,
            z,
            row_shape,
            layer_idx,
            attrs,
            records,
            all_rings,
            geom_buf,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_gap_run(
    feature_id: u64,
    tx_min: u32,
    tx_max: u32,
    ty: u32,
    z: u8,
    row_shape: &Shape,
    layer_idx: u8,
    attrs: &[shortbread::Attr],
    records: &mut Vec<SortRecord>,
    all_rings: &mut Vec<Vec<(i32, i32)>>,
    geom_buf: &mut Vec<u32>,
) {
    if tx_min > tx_max {
        return;
    }
    let cy = tile_center_coord(ty);
    let test_x = tile_center_coord(tx_min);
    if !point_in_shape(test_x, cy, row_shape) {
        return;
    }
    for tx in tx_min..=tx_max {
        emit_clipped_tile_shape(
            feature_id, tx, ty, z, row_shape, layer_idx, attrs, records, all_rings, geom_buf,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_tile_for_row_shapes(
    feature_id: u64,
    tx: u32,
    ty: u32,
    z: u8,
    row_shapes: &Shapes,
    layer_idx: u8,
    attrs: &[shortbread::Attr],
    records: &mut Vec<SortRecord>,
    all_rings: &mut Vec<Vec<(i32, i32)>>,
    geom_buf: &mut Vec<u32>,
) {
    for row_shape in row_shapes {
        emit_clipped_tile_shape(
            feature_id, tx, ty, z, row_shape, layer_idx, attrs, records, all_rings, geom_buf,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_clipped_tile_shape(
    feature_id: u64,
    tx: u32,
    ty: u32,
    z: u8,
    shape: &Shape,
    layer_idx: u8,
    attrs: &[shortbread::Attr],
    records: &mut Vec<SortRecord>,
    all_rings: &mut Vec<Vec<(i32, i32)>>,
    geom_buf: &mut Vec<u32>,
) {
    let tile_shapes = intersect_rect(shape, buffered_tile_rect(tx, ty), 256);
    if tile_shapes.is_empty() {
        return;
    }

    let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
    let key_base = sort::make_sort_key(tile_id, layer_idx, 0);
    let ox = tile_origin(tx);
    let oy = tile_origin(ty);

    for tile_shape in tile_shapes {
        all_rings.clear();
        for (i, contour) in tile_shape.into_iter().enumerate() {
            if contour.len() < 3 {
                continue;
            }
            let mut ring: Vec<(i32, i32)> =
                contour.into_iter().map(|p| (p.x - ox, p.y - oy)).collect();
            if i == 0 {
                close_and_orient_cw(&mut ring);
            } else {
                close_and_orient_ccw(&mut ring);
            }
            all_rings.push(ring);
        }
        if all_rings.is_empty() {
            continue;
        }

        let ring_refs: Vec<&[(i32, i32)]> = all_rings.iter().map(Vec::as_slice).collect();
        mvt::encode_polygon(geom_buf, &ring_refs);
        if geom_buf.is_empty() {
            continue;
        }
        let data = encode_feature_data(feature_id, GeomType::Polygon, geom_buf, attrs, z);
        records.push(SortRecord { key: key_base, data });
    }
}

/// Emit a full-tile ocean rectangle for pure-ocean tiles (no coastline).
#[allow(clippy::too_many_arguments)]
fn emit_full_tile(
    feature_id: u64,
    tx: u32, ty: u32, z: u8,
    layer_idx: u8,
    attrs: &[shortbread::Attr],
    records: &mut Vec<SortRecord>,
    all_rings: &mut Vec<Vec<(i32, i32)>>,
    geom_buf: &mut Vec<u32>,
) {
    #[allow(clippy::cast_possible_truncation)]
    let buf = (BUFFER_FRACTION * geometry::EXTENT) as i32;
    #[allow(clippy::cast_possible_truncation)]
    let ext = geometry::EXTENT as i32;
    let ring = vec![
        (-buf, -buf), (ext + buf, -buf), (ext + buf, ext + buf), (-buf, ext + buf), (-buf, -buf),
    ];
    all_rings.clear();
    all_rings.push(ring);
    mvt::encode_polygon(geom_buf, &[all_rings[0].as_slice()]);
    if geom_buf.is_empty() { return; }
    let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
    let key_base = sort::make_sort_key(tile_id, layer_idx, 0);
    let data = encode_feature_data(feature_id, GeomType::Polygon, geom_buf, attrs, z);
    records.push(SortRecord { key: key_base, data });
}

// ---------------------------------------------------------------------------
// Rasterization helpers
// ---------------------------------------------------------------------------

/// Rasterize polygon ring edges into tile grid cells using DDA grid traversal.
/// Marks all tiles that a ring's edges cross through.
fn rasterize_shape_edges(shape: &Shape, max_tile: u32, tiles: &mut HashSet<u64>) {
    for ring in shape {
        if ring.len() < 2 {
            continue;
        }
        for i in 0..ring.len() {
            let j = if i + 1 < ring.len() { i + 1 } else { 0 };
            let x0 = f64::from(ring[i].x) / f64::from(TILE_EXTENT_I32);
            let y0 = f64::from(ring[i].y) / f64::from(TILE_EXTENT_I32);
            let x1 = f64::from(ring[j].x) / f64::from(TILE_EXTENT_I32);
            let y1 = f64::from(ring[j].y) / f64::from(TILE_EXTENT_I32);
            rasterize_segment_clamped(x0, y0, x1, y1, max_tile, tiles);
        }
    }
}

/// DDA grid traversal: enumerate all grid cells a line segment crosses.
#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rasterize_segment(
    x0: f64, y0: f64, x1: f64, y1: f64,
    tiles: &mut HashSet<u64>,
) {
    rasterize_segment_clamped(x0, y0, x1, y1, u32::MAX, tiles);
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rasterize_segment_clamped(
    x0: f64, y0: f64, x1: f64, y1: f64,
    max_tile: u32,
    tiles: &mut HashSet<u64>,
) {
    let mut cx = x0.floor() as i32;
    let mut cy = y0.floor() as i32;
    let ex = x1.floor() as i32;
    let ey = y1.floor() as i32;

    insert_rasterized_tile(cx, cy, max_tile, tiles);

    let dx = x1 - x0;
    let dy = y1 - y0;

    if dx == 0.0 && dy == 0.0 {
        return;
    }

    if dy == 0.0 && is_grid_line_coord(y0) {
        let step_x = if dx > 0.0 { 1 } else { -1 };
        rasterize_horizontal_grid_line(cx, cy, ex, step_x, max_tile, tiles);
        return;
    }

    if dx == 0.0 && is_grid_line_coord(x0) {
        let step_y = if dy > 0.0 { 1 } else { -1 };
        rasterize_vertical_grid_line(cx, cy, ey, step_y, max_tile, tiles);
        return;
    }

    let step_x: i32 = if dx > 0.0 { 1 } else { -1 };
    let step_y: i32 = if dy > 0.0 { 1 } else { -1 };

    let mut t_max_x = if dx != 0.0 {
        let next_x = if dx > 0.0 { (cx + 1) as f64 } else { cx as f64 };
        (next_x - x0) / dx
    } else {
        f64::MAX
    };
    let mut t_max_y = if dy != 0.0 {
        let next_y = if dy > 0.0 { (cy + 1) as f64 } else { cy as f64 };
        (next_y - y0) / dy
    } else {
        f64::MAX
    };

    let t_delta_x = if dx != 0.0 { (step_x as f64) / dx } else { f64::MAX };
    let t_delta_y = if dy != 0.0 { (step_y as f64) / dy } else { f64::MAX };

    // Walk until we reach the end tile
    let max_steps = (cx - ex).unsigned_abs() + (cy - ey).unsigned_abs() + 2;
    for _ in 0..max_steps {
        if cx == ex && cy == ey {
            break;
        }
        // At exact grid-corner crossings the segment touches both side cells
        // before entering the diagonal cell. Over-marking boundary tiles is
        // safe; under-marking can misclassify a boundary row as an interior gap.
        if t_max_x.total_cmp(&t_max_y).is_eq() {
            insert_rasterized_tile(cx + step_x, cy, max_tile, tiles);
            insert_rasterized_tile(cx, cy + step_y, max_tile, tiles);
            cx += step_x;
            cy += step_y;
            t_max_x += t_delta_x;
            t_max_y += t_delta_y;
        } else if t_max_x < t_max_y {
            cx += step_x;
            t_max_x += t_delta_x;
        } else {
            cy += step_y;
            t_max_y += t_delta_y;
        }
        insert_rasterized_tile(cx, cy, max_tile, tiles);
    }
}

#[inline]
fn is_grid_line_coord(coord: f64) -> bool {
    coord.fract().abs() <= f64::EPSILON
}

fn rasterize_horizontal_grid_line(
    mut cx: i32,
    cy: i32,
    ex: i32,
    step_x: i32,
    max_tile: u32,
    tiles: &mut HashSet<u64>,
) {
    let max_steps = (cx - ex).unsigned_abs() + 2;
    for _ in 0..max_steps {
        insert_rasterized_tile(cx, cy, max_tile, tiles);
        insert_rasterized_tile(cx, cy - 1, max_tile, tiles);
        if cx == ex {
            break;
        }
        cx += step_x;
    }
}

fn rasterize_vertical_grid_line(
    cx: i32,
    mut cy: i32,
    ey: i32,
    step_y: i32,
    max_tile: u32,
    tiles: &mut HashSet<u64>,
) {
    let max_steps = (cy - ey).unsigned_abs() + 2;
    for _ in 0..max_steps {
        insert_rasterized_tile(cx, cy, max_tile, tiles);
        insert_rasterized_tile(cx - 1, cy, max_tile, tiles);
        if cy == ey {
            break;
        }
        cy += step_y;
    }
}

#[inline]
fn insert_rasterized_tile(tx: i32, ty: i32, max_tile: u32, tiles: &mut HashSet<u64>) {
    if let (Ok(tx), Ok(ty)) = (u32::try_from(tx), u32::try_from(ty)) {
        tiles.insert(pack_tile(tx.min(max_tile), ty.min(max_tile)));
    }
}

#[inline]
fn pack_tile(tx: u32, ty: u32) -> u64 {
    (u64::from(tx) << 32) | u64::from(ty)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use crate::geometry::Point;
    use crate::sort::SortWriter;

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
        write_u32_le(&mut shp, rec + 36, u32::try_from(num_parts).expect("part count fits u32")); // num_parts
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
    // rasterize_segment tests
    // -----------------------------------------------------------------------

    fn packed_tiles<const N: usize>(tiles: [(u32, u32); N]) -> HashSet<u64> {
        tiles
            .into_iter()
            .map(|(tx, ty)| pack_tile(tx, ty))
            .collect()
    }

    fn ip(x: i32, y: i32) -> i_overlay::i_float::int::point::IntPoint {
        i_overlay::i_float::int::point::IntPoint::new(x, y)
    }

    fn rect_row_shape(rect: IntRect) -> Shapes {
        vec![vec![vec![
            ip(rect.min_x, rect.min_y),
            ip(rect.max_x, rect.min_y),
            ip(rect.max_x, rect.max_y),
            ip(rect.min_x, rect.max_y),
        ]]]
    }

    #[test]
    fn fast_path_fires_for_single_4_corner_rect() {
        let bbox = IntRect { min_x: 0, min_y: 0, max_x: 12_288, max_y: 12_288 };
        let band = IntRect { min_x: 0, min_y: 3_968, max_x: 12_288, max_y: 8_320 };
        let row_shapes = rect_row_shape(rect_intersection(bbox, band).unwrap());
        assert_eq!(fast_path_rect(&row_shapes, bbox, band), rect_intersection(bbox, band));
    }

    #[test]
    fn fast_path_rejects_one_vertex_displaced_by_1_unit() {
        let bbox = IntRect { min_x: 0, min_y: 0, max_x: 12_288, max_y: 12_288 };
        let band = IntRect { min_x: 0, min_y: 3_968, max_x: 12_288, max_y: 8_320 };
        let rect = rect_intersection(bbox, band).unwrap();
        let row_shapes = vec![vec![vec![
            ip(rect.min_x, rect.min_y),
            ip(rect.max_x + 1, rect.min_y),
            ip(rect.max_x, rect.max_y),
            ip(rect.min_x, rect.max_y),
        ]]];
        assert!(fast_path_rect(&row_shapes, bbox, band).is_none());
    }

    #[test]
    fn fast_path_rejects_extra_contour_hole() {
        let bbox = IntRect { min_x: 0, min_y: 0, max_x: 12_288, max_y: 12_288 };
        let band = IntRect { min_x: 0, min_y: 3_968, max_x: 12_288, max_y: 8_320 };
        let rect = rect_intersection(bbox, band).unwrap();
        let mut row_shapes = rect_row_shape(rect);
        row_shapes[0].push(vec![ip(100, 4_100), ip(200, 4_100), ip(200, 4_200), ip(100, 4_200)]);
        assert!(fast_path_rect(&row_shapes, bbox, band).is_none());
    }

    #[test]
    fn fast_path_edge_tiles_whose_buffered_rect_exceeds_r_take_boolean_path() {
        let rect = IntRect { min_x: 0, min_y: -128, max_x: 12_288, max_y: 4_224 };
        assert!(!buffered_tile_rect_contained(0, 0, rect));
        assert!(buffered_tile_rect_contained(1, 0, rect));
        assert!(!buffered_tile_rect_contained(2, 0, rect));
    }

    #[test]
    fn fast_path_rotated_reversed_corner_order_still_fires() {
        let bbox = IntRect { min_x: 0, min_y: 0, max_x: 12_288, max_y: 12_288 };
        let band = IntRect { min_x: 0, min_y: 3_968, max_x: 12_288, max_y: 8_320 };
        let rect = rect_intersection(bbox, band).unwrap();
        let row_shapes = vec![vec![vec![
            ip(rect.max_x, rect.max_y),
            ip(rect.max_x, rect.min_y),
            ip(rect.min_x, rect.min_y),
            ip(rect.min_x, rect.max_y),
        ]]];
        assert_eq!(fast_path_rect(&row_shapes, bbox, band), Some(rect));
    }

    #[test]
    fn rasterize_horizontal() {
        let mut tiles = HashSet::new();
        rasterize_segment(0.5, 0.5, 3.5, 0.5, &mut tiles);
        let expected: HashSet<u64> = [
            pack_tile(0, 0),
            pack_tile(1, 0),
            pack_tile(2, 0),
            pack_tile(3, 0),
        ]
        .into_iter()
        .collect();
        assert_eq!(tiles, expected);
    }

    #[test]
    fn rasterize_vertical() {
        let mut tiles = HashSet::new();
        rasterize_segment(0.5, 0.5, 0.5, 3.5, &mut tiles);
        let expected: HashSet<u64> = [
            pack_tile(0, 0),
            pack_tile(0, 1),
            pack_tile(0, 2),
            pack_tile(0, 3),
        ]
        .into_iter()
        .collect();
        assert_eq!(tiles, expected);
    }

    #[test]
    fn rasterize_diagonal() {
        let mut tiles = HashSet::new();
        rasterize_segment(0.5, 0.5, 2.5, 2.5, &mut tiles);
        let expected = packed_tiles([(0, 0), (1, 0), (0, 1), (1, 1), (2, 1), (1, 2), (2, 2)]);
        assert_eq!(tiles, expected);
    }

    #[test]
    fn rasterize_45_degree_segment_through_exact_corners_marks_both_side_cells() {
        let mut tiles = HashSet::new();
        rasterize_segment(0.5, 0.5, 2.5, 2.5, &mut tiles);
        let expected = packed_tiles([(0, 0), (1, 0), (0, 1), (1, 1), (2, 1), (1, 2), (2, 2)]);
        assert_eq!(tiles, expected);
    }

    #[test]
    fn rasterize_segment_starting_exactly_on_a_corner() {
        let mut tiles = HashSet::new();
        rasterize_segment(1.0, 1.0, 3.0, 3.0, &mut tiles);
        let expected = packed_tiles([(1, 1), (2, 1), (1, 2), (2, 2), (3, 2), (2, 3), (3, 3)]);
        assert_eq!(tiles, expected);
    }

    #[test]
    fn rasterize_segment_ending_exactly_on_a_corner() {
        let mut tiles = HashSet::new();
        rasterize_segment(0.5, 0.5, 2.0, 2.0, &mut tiles);
        let expected = packed_tiles([(0, 0), (1, 0), (0, 1), (1, 1), (2, 1), (1, 2), (2, 2)]);
        assert_eq!(tiles, expected);
    }

    #[test]
    fn rasterize_axis_aligned_segment_along_a_grid_line() {
        let mut tiles = HashSet::new();
        rasterize_segment(0.5, 1.0, 3.5, 1.0, &mut tiles);
        let expected = packed_tiles([
            (0, 0),
            (0, 1),
            (1, 0),
            (1, 1),
            (2, 0),
            (2, 1),
            (3, 0),
            (3, 1),
        ]);
        assert_eq!(tiles, expected);
    }

    #[test]
    fn rasterize_tangent_corner_touch() {
        let mut tiles = HashSet::new();
        rasterize_segment(0.5, 1.5, 1.5, 0.5, &mut tiles);
        let expected = packed_tiles([(0, 1), (1, 1), (0, 0), (1, 0)]);
        assert_eq!(tiles, expected);
    }

    #[test]
    fn rasterize_zero_length() {
        let mut tiles = HashSet::new();
        rasterize_segment(1.5, 2.5, 1.5, 2.5, &mut tiles);
        assert_eq!(tiles.len(), 1);
        assert!(tiles.contains(&pack_tile(1, 2)));
    }

    #[test]
    fn rasterize_negative_coords() {
        let mut tiles = HashSet::new();
        rasterize_segment(-1.5, -0.5, 1.5, 0.5, &mut tiles);
        // Only tiles with cx >= 0 && cy >= 0 are inserted
        for &packed in &tiles {
            let tx = packed >> 32;
            let ty = packed & 0xFFFF_FFFF;
            assert!(tx < 0x8000_0000, "negative tx snuck in");
            assert!(ty < 0x8000_0000, "negative ty snuck in");
        }
        // The segment enters the non-negative quadrant, so at least one tile
        assert!(!tiles.is_empty());
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
        assert!(geometry::point_in_polygon(&Point::new(0.5, 0.5), &unit_square()));
    }

    #[test]
    fn pip_outside_square() {
        assert!(!geometry::point_in_polygon(&Point::new(2.0, 0.5), &unit_square()));
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
        assert!(!geometry::point_in_polygon(&Point::new(0.0, 0.0), &[Point { x: 0.0, y: 0.0 }]));
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

        let mut sort_writer = SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };

        let emitted = process_ocean_shapefile(
            &shp_path,
            &bounds,
            0,
            0,
            &mut sort_writer,
        )
        .unwrap();

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

        let mut sort_writer = SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        // Mercator data bounds outside [0,1] should not intersect any valid projected shape.
        let disjoint_bounds = MercBbox {
            min_x: 2.0,
            min_y: 2.0,
            max_x: 3.0,
            max_y: 3.0,
        };

        let emitted = process_ocean_shapefile(
            &shp_path,
            &disjoint_bounds,
            0,
            0,
            &mut sort_writer,
        )
        .unwrap();

        assert_eq!(emitted, 0, "expected no ocean features for disjoint bounds");
    }

    #[test]
    fn process_ocean_shapefile_handles_polygon_with_hole() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_test_hole.shp");
        write_test_polygon_with_hole_shapefile(&shp_path);

        let mut sort_writer = SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };

        let emitted = process_ocean_shapefile(
            &shp_path,
            &bounds,
            0,
            0,
            &mut sort_writer,
        )
        .unwrap();
        assert!(emitted > 0, "expected ocean features to be emitted");

        let mut reader = sort_writer.finish().unwrap();
        let rec = reader.next().unwrap().expect("expected at least one record");
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

        let mut sort_writer = SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let err = process_ocean_shapefile(
            &shp_path,
            &bounds,
            0,
            0,
            &mut sort_writer,
        )
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

        let mut sort_writer = SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let err = process_ocean_shapefile(
            &shp_path,
            &bounds,
            0,
            0,
            &mut sort_writer,
        )
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

        let mut sort_writer = SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let emitted = process_ocean_shapefile(
            &shp_path,
            &bounds,
            0,
            0,
            &mut sort_writer,
        )
        .unwrap();
        assert_eq!(emitted, 0, "truncated record should be skipped");
    }
}
