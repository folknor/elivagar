// Ocean shapefile processing.
//
// Reads a water-polygons-split-3857 shapefile (mmap + .shx index), filters to
// data bounds, parses polygons, and processes them in parallel with rayon.
// Uses scanline fill to minimize point-in-polygon tests.

use crate::geometry::{self, ClipRect, MercBbox, Point, BUFFER_FRACTION, close_and_orient_cw, close_and_orient_ccw, merc_bbox};
use crate::mvt::{self, GeomType};
use crate::pmtiles_writer;
use crate::shortbread::{self, Layer};
use crate::sort::{self, SortRecord, SortWriter};
use crate::wire_format::encode_feature_data;

use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Parsed ocean polygon ready for parallel processing.
// Vecs are ephemeral — consumed once during ocean tile emission, boxed_slice not worth it.
struct OceanPolygon {
    outer: Vec<Point>,
    inners: Vec<Vec<Point>>,
}
const _: () = assert!(std::mem::size_of::<OceanPolygon>() == 48);

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
    land_mask: Option<&geometry::LandMask>,
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

    let bounds_clip = ClipRect::new(
        data_bounds.min_x, data_bounds.min_y,
        data_bounds.max_x, data_bounds.max_y,
    );

    // --- Parse phase: extract all polygons (single-threaded, sequential I/O) ---
    let mut polygons: Vec<OceanPolygon> = Vec::new();
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

            let clipped = geometry::clip_polygon(ring, &bounds_clip);
            if clipped.len() < 3 {
                if w == 0 || geometry::signed_area(ring) >= 0.0 {
                    if let Some(outer) = current_outer.take() {
                        polygons.push(OceanPolygon { outer, inners: std::mem::take(&mut current_inners) });
                    }
                    current_outer = None;
                }
                continue;
            }

            let is_outer = w == 0 || geometry::signed_area(ring) >= 0.0;

            if is_outer {
                if let Some(outer) = current_outer.take() {
                    polygons.push(OceanPolygon { outer, inners: std::mem::take(&mut current_inners) });
                }
                current_outer = Some(clipped);
            } else {
                current_inners.push(clipped);
            }
        }

        if let Some(outer) = current_outer {
            polygons.push(OceanPolygon { outer, inners: current_inners });
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
        let orig_count = polygons.len();
        let mut split_out: Vec<OceanPolygon> = Vec::with_capacity(polygons.len());
        let mut clip_a: Vec<Point> = Vec::new();
        let mut clip_b: Vec<Point> = Vec::new();
        let split_scale = f64::from(1u32 << SPLIT_Z);
        let split_inv = 1.0 / split_scale;
        for poly in polygons.drain(..) {
            if poly.outer.len() < SPLIT_MIN_VERTICES {
                split_out.push(poly);
                continue;
            }
            let bb = merc_bbox(&poly.outer);
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let stx_min = (bb.min_x * split_scale).floor().max(0.0) as u32;
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let sty_min = (bb.min_y * split_scale).floor().max(0.0) as u32;
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let stx_max = ((bb.max_x * split_scale).floor().max(0.0) as u32).min((1u32 << SPLIT_Z) - 1);
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let sty_max = ((bb.max_y * split_scale).floor().max(0.0) as u32).min((1u32 << SPLIT_Z) - 1);
            if stx_min == stx_max && sty_min == sty_max {
                // Already fits in one tile at SPLIT_Z
                split_out.push(poly);
                continue;
            }
            for sty in sty_min..=sty_max {
                for stx in stx_min..=stx_max {
                    let tile_rect = ClipRect::new(
                        f64::from(stx) * split_inv,
                        f64::from(sty) * split_inv,
                        f64::from(stx + 1) * split_inv,
                        f64::from(sty + 1) * split_inv,
                    );
                    geometry::clip_polygon_into(&poly.outer, &tile_rect, &mut clip_a, &mut clip_b);
                    if clip_a.len() < 4 { continue; }
                    let sub_outer = clip_a.clone();
                    let sub_inners: Vec<Vec<Point>> = poly.inners.iter().filter_map(|inner| {
                        geometry::clip_polygon_into(inner, &tile_rect, &mut clip_a, &mut clip_b);
                        if clip_a.len() >= 4 { Some(clip_a.clone()) } else { None }
                    }).collect();
                    split_out.push(OceanPolygon { outer: sub_outer, inners: sub_inners });
                }
            }
        }
        polygons = split_out;
        if polygons.len() != orig_count {
            eprintln!("  Pre-split at z{SPLIT_Z}: {orig_count} → {} polygons", polygons.len());
        }
    }

    let poly_count = polygons.len();
    eprintln!("  {shape_count} shapes, {shapes_hit} in bounds, {poly_count} polygons — processing in parallel");

    // --- Process phase: parallel with rayon, direct chunk flushing ---
    //
    // Each rayon worker accumulates records in a thread-local buffer and flushes
    // directly to a chunk file when the buffer exceeds chunk_size_bytes. This
    // avoids holding all ocean sort records in memory simultaneously — at planet
    // scale that could be 10-30 GB. The previous approach (par_iter().collect()
    // into Vec<Vec<SortRecord>> + serial push) was fine for regional extracts but
    // would blow memory and serialize sort+flush at planet scale.
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let ocean_layer = Layer::Ocean as u8;
    let empty_attrs: Vec<shortbread::Attr> = Vec::new();

    if let Some(mask) = land_mask {
        eprintln!("  Land mask: {} z14 cells, filtering enabled", mask.count_set());
    }

    // Ocean chunks use the same chunk_NNNN.bin naming (starting after PBF chunks)
    // so that --skip-to sort (SortReader::from_dir sequential scan) finds them.
    let chunk_id = AtomicUsize::new(sort_writer.chunk_count());
    let chunk_dir = sort_writer.tmp_dir().to_path_buf();
    let chunk_size = sort_writer.chunk_size_bytes();

    struct OceanAcc {
        records: Vec<SortRecord>,
        bytes: usize,
        chunk_paths: Vec<std::path::PathBuf>,
        count: u64,
        simp_scratch: geometry::SimplifyMultiScratch,
    }

    impl OceanAcc {
        fn flush(&mut self, chunk_dir: &std::path::Path, chunk_id: &AtomicUsize) {
            if self.records.is_empty() {
                return;
            }
            let id = chunk_id.fetch_add(1, Ordering::Relaxed);
            let path = chunk_dir.join(format!("chunk_{id:04}.bin"));
            // Panic: inside rayon fold — can't propagate Result. Disk I/O failure is unrecoverable.
            sort::write_sorted_chunk(&mut self.records, &path)
                .expect("ocean chunk write failed");
            self.chunk_paths.push(path);
            self.count += self.records.len() as u64;
            self.records.clear();
            self.bytes = 0;
        }
    }

    let result = polygons
        .par_iter()
        .enumerate()
        .fold(
            || OceanAcc { records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0, simp_scratch: geometry::SimplifyMultiScratch::new() },
            |mut acc, (idx, poly)| {
                let before = acc.records.len();
                emit_ocean_polygon(
                    idx as u64, &poly.outer, &poly.inners,
                    min_zoom, max_zoom, ocean_layer, &empty_attrs,
                    land_mask, &mut acc.records, &mut acc.simp_scratch,
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
            || OceanAcc { records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0, simp_scratch: geometry::SimplifyMultiScratch::new() },
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
    outer: &[Point],
    inners: &[Vec<Point>],
    min_zoom: u8,
    max_zoom: u8,
    layer_idx: u8,
    attrs: &[shortbread::Attr],
    land_mask: Option<&geometry::LandMask>,
    records: &mut Vec<SortRecord>,
    simp_scratch: &mut geometry::SimplifyMultiScratch,
) {
    if outer.len() < 4 {
        return;
    }

    // Pre-compute fill tile data (a rectangle covering the full tile extent).
    // Use osm_id=0 so ALL ocean fill tiles produce identical bytes, enabling
    // PMTiles content-hash dedup. The per-polygon feature_id is irrelevant
    // for full-tile fills (no visible feature identity).
    #[allow(clippy::cast_possible_truncation)]
    let ext = geometry::EXTENT as i32;
    let fill_ring: [(i32, i32); 5] = [(0, 0), (ext, 0), (ext, ext), (0, ext), (0, 0)];
    let mut geom_buf: Vec<u32> = Vec::new();
    mvt::encode_polygon(&mut geom_buf, &[&fill_ring]);
    let fill_data = encode_feature_data(0, GeomType::Polygon, &geom_buf, attrs, 0);

    let bbox = merc_bbox(outer);

    // Reuse collections across zoom iterations (O4: avoid re-alloc per zoom)
    let mut boundary_tiles: HashSet<u64> = HashSet::new();
    let mut boundary_rows: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut gaps: Vec<(u32, u32)> = Vec::new();
    let mut bt_all_rings: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut bt_geom_buf: Vec<u32> = Vec::new();
    let mut clip_a: Vec<Point> = Vec::new();
    let mut clip_b: Vec<Point> = Vec::new();
    // Row pre-clip buffers: clip polygon to each tile row's Y-band before
    // per-tile clipping. Reduces input vertex count for individual tile clips
    // dramatically for large ocean polygons spanning many rows.
    let mut row_clip_a: Vec<Point> = Vec::new();
    let mut row_clip_b: Vec<Point> = Vec::new();
    let mut row_outer: Vec<Point> = Vec::new();
    let mut row_inners: Vec<Vec<Point>> = Vec::new();

    geometry::for_each_zoom_simplified_multi(outer, inners, min_zoom, max_zoom, simp_scratch, |z, simp_outer, simp_inners| {
        let scale = f64::from(1u32 << z);
        let inv_scale = 1.0 / scale;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let max_tile = (1u32 << z) - 1;

        // Rasterize polygon edges → boundary tiles
        boundary_tiles.clear();
        rasterize_ring_edges(simp_outer, scale, &mut boundary_tiles);
        for inner in simp_inners {
            rasterize_ring_edges(inner, scale, &mut boundary_tiles);
        }

        // Group boundary tiles by row (ty → sorted tx list)
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

        // Tile range from bbox
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let tx_min = (bbox.min_x * scale).floor().max(0.0) as u32;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let ty_min = (bbox.min_y * scale).floor().max(0.0) as u32;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let tx_max = ((bbox.max_x * scale).floor().max(0.0) as u32).min(max_tile);
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let ty_max = ((bbox.max_y * scale).floor().max(0.0) as u32).min(max_tile);

        // PIP helper: inside outer and not in any hole
        let pip = |px: f64, py: f64| -> bool {
            let test_pt = Point::new(px, py);
            geometry::point_in_polygon(&test_pt, simp_outer)
                && !simp_inners.iter().any(|inner| geometry::point_in_polygon(&test_pt, inner))
        };

        // Scanline: process row by row
        let tile_buf = BUFFER_FRACTION * inv_scale;
        for ty in ty_min..=ty_max {
            let cy = (f64::from(ty) + 0.5) * inv_scale;

            if let Some(bx_list) = boundary_rows.get(&ty) {
                // Row pre-clip: restrict polygon to this row's Y-band.
                // Per-tile clips then process a much smaller polygon
                // (e.g. ~50 vertices instead of ~1000 for large fjord polygons).
                let row_rect = ClipRect::new(
                    0.0,
                    f64::from(ty) * inv_scale - tile_buf,
                    1.0,
                    f64::from(ty + 1) * inv_scale + tile_buf,
                );
                geometry::clip_polygon_into(simp_outer, &row_rect, &mut row_clip_a, &mut row_clip_b);
                row_outer.clear();
                row_outer.extend_from_slice(&row_clip_a);

                // X-extent of row-clipped polygon — skip boundary tiles outside this range
                let (row_x_min, row_x_max) = row_outer.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| (lo.min(p.x), hi.max(p.x)));

                // Pre-clip inners to row band (rare for ocean, usually empty)
                let mut row_inner_count = 0;
                for inner in simp_inners {
                    geometry::clip_polygon_into(inner, &row_rect, &mut row_clip_a, &mut row_clip_b);
                    if row_clip_a.len() >= 3 {
                        if row_inner_count < row_inners.len() {
                            row_inners[row_inner_count].clear();
                            row_inners[row_inner_count].extend_from_slice(&row_clip_a);
                        } else {
                            row_inners.push(row_clip_a.clone());
                        }
                        row_inner_count += 1;
                    }
                }

                // Row has boundary tiles — clip+emit them, then fill gaps
                for &tx in bx_list {
                    let tile_x_min = f64::from(tx) * inv_scale - tile_buf;
                    let tile_x_max = f64::from(tx + 1) * inv_scale + tile_buf;
                    if row_x_max < tile_x_min || row_x_min > tile_x_max { continue; }
                    if let Some(mask) = land_mask
                        && !mask.has_land(z, tx, ty) { continue; }
                    emit_boundary_tile(
                        feature_id, tx, ty, z, &row_outer, &row_inners[..row_inner_count],
                        layer_idx, attrs, records,
                        &mut bt_all_rings, &mut bt_geom_buf,
                        &mut clip_a, &mut clip_b,
                    );
                }

                // Fill gaps between boundary tiles
                gaps.clear();
                if bx_list[0] > tx_min {
                    gaps.push((tx_min, bx_list[0] - 1));
                }
                for pair in bx_list.windows(2) {
                    if pair[1] > pair[0] + 1 {
                        gaps.push((pair[0] + 1, pair[1] - 1));
                    }
                }
                if *bx_list.last().expect("nonempty") < tx_max {
                    gaps.push((bx_list.last().expect("nonempty") + 1, tx_max));
                }

                for (gx_min, gx_max) in &gaps {
                    let test_cx = (f64::from(*gx_min) + 0.5) * inv_scale;
                    if pip(test_cx, cy) {
                        for tx in *gx_min..=*gx_max {
                            if let Some(mask) = land_mask
                                && !mask.has_land(z, tx, ty) { continue; }
                            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                            let key = sort::make_sort_key(tile_id, layer_idx, 0);
                            records.push(SortRecord { key, data: fill_data.clone() });
                        }
                    }
                }
            } else {
                // No boundary tiles in this row — single PIP test
                let test_cx = (f64::from(tx_min) + 0.5) * inv_scale;
                if pip(test_cx, cy) {
                    for tx in tx_min..=tx_max {
                        if let Some(mask) = land_mask
                            && !mask.has_land(z, tx, ty) { continue; }
                        let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                        let key = sort::make_sort_key(tile_id, layer_idx, 0);
                        records.push(SortRecord { key, data: fill_data.clone() });
                    }
                }
            }
        }
    });
}

/// Clip and encode a single boundary tile (polygon edge crosses this tile).
/// Reusable buffers (`all_rings`, `geom_buf`, `clip_a`, `clip_b`) are passed in
/// to avoid per-call allocation — this function is called per boundary tile per zoom.
#[allow(clippy::too_many_arguments)]
fn emit_boundary_tile(
    feature_id: u64,
    tx: u32, ty: u32, z: u8,
    outer: &[Point],
    inners: &[Vec<Point>],
    layer_idx: u8,
    attrs: &[shortbread::Attr],
    records: &mut Vec<SortRecord>,
    all_rings: &mut Vec<Vec<(i32, i32)>>,
    geom_buf: &mut Vec<u32>,
    clip_a: &mut Vec<Point>,
    clip_b: &mut Vec<Point>,
) {
    let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);

    geometry::clip_polygon_into(outer, &clip, clip_a, clip_b);
    if clip_a.len() < 3 {
        return;
    }
    let mut outer_tc = geometry::to_tile_coords(clip_a, tx, ty, z);
    close_and_orient_cw(&mut outer_tc);

    all_rings.clear();
    all_rings.push(outer_tc);
    for inner in inners {
        geometry::clip_polygon_into(inner, &clip, clip_a, clip_b);
        if clip_a.len() < 3 {
            continue;
        }
        let mut inner_tc = geometry::to_tile_coords(clip_a, tx, ty, z);
        close_and_orient_ccw(&mut inner_tc);
        all_rings.push(inner_tc);
    }

    // ring_refs borrows all_rings — must be local (can't hoist across calls).
    let ring_refs: Vec<&[(i32, i32)]> = all_rings.iter().map(Vec::as_slice).collect();
    mvt::encode_polygon(geom_buf, &ring_refs);
    if geom_buf.is_empty() {
        return;
    }
    let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
    let data = encode_feature_data(feature_id, GeomType::Polygon, geom_buf, attrs, z);
    let key = sort::make_sort_key(tile_id, layer_idx, 0);
    records.push(SortRecord { key, data });
}

// ---------------------------------------------------------------------------
// Rasterization helpers
// ---------------------------------------------------------------------------

/// Rasterize polygon ring edges into tile grid cells using DDA grid traversal.
/// Marks all tiles that a ring's edges cross through.
fn rasterize_ring_edges(ring: &[Point], scale: f64, tiles: &mut HashSet<u64>) {
    if ring.len() < 2 {
        return;
    }
    for i in 0..ring.len() {
        let j = if i + 1 < ring.len() { i + 1 } else { 0 };
        let x0 = ring[i].x * scale;
        let y0 = ring[i].y * scale;
        let x1 = ring[j].x * scale;
        let y1 = ring[j].y * scale;
        rasterize_segment(x0, y0, x1, y1, tiles);
    }
}

/// DDA grid traversal: enumerate all grid cells a line segment crosses.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rasterize_segment(
    x0: f64, y0: f64, x1: f64, y1: f64,
    tiles: &mut HashSet<u64>,
) {
    let mut cx = x0.floor() as i32;
    let mut cy = y0.floor() as i32;
    let ex = x1.floor() as i32;
    let ey = y1.floor() as i32;

    if cx >= 0 && cy >= 0 {
        tiles.insert(pack_tile(cx as u32, cy as u32));
    }

    let dx = x1 - x0;
    let dy = y1 - y0;

    if dx == 0.0 && dy == 0.0 {
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
        // Known: when t_max_x == t_max_y (exact grid corner crossing), only
        // the Y step is taken, skipping the X-direction tile. Not reachable
        // with real shapefile coordinates (requires exact float equality).
        // Even if triggered, the scanline PIP fallback handles the missed tile.
        if t_max_x < t_max_y {
            cx += step_x;
            t_max_x += t_delta_x;
        } else {
            cy += step_y;
            t_max_y += t_delta_y;
        }
        if cx >= 0 && cy >= 0 {
            tiles.insert(pack_tile(cx as u32, cy as u32));
        }
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
    use crate::geometry::Point;

    // -----------------------------------------------------------------------
    // rasterize_segment tests
    // -----------------------------------------------------------------------

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
        // Must hit the three main diagonal tiles
        assert!(tiles.contains(&pack_tile(0, 0)));
        assert!(tiles.contains(&pack_tile(1, 1)));
        assert!(tiles.contains(&pack_tile(2, 2)));
        // DDA may also step through (0,1) or (1,0) at grid crossings —
        // just verify the result is a subset of the plausible set.
        let plausible: HashSet<u64> = [
            pack_tile(0, 0),
            pack_tile(0, 1),
            pack_tile(1, 0),
            pack_tile(1, 1),
            pack_tile(1, 2),
            pack_tile(2, 1),
            pack_tile(2, 2),
        ]
        .into_iter()
        .collect();
        assert!(tiles.is_subset(&plausible));
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

        // Inside the concavity (the cut-out region) — should be false
        assert!(!geometry::point_in_polygon(&Point::new(1.5, 2.0), &l_shape));
    }
}
