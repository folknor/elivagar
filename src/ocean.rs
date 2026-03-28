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

/// Minimum ring area in extent² units (2x signed area threshold).
/// 1 pixel² = 16² = 256 extent² units (EXTENT=4096, 256 pixels per tile).
/// We compare against 2x area (shoelace without /2), so threshold is 512.
const MIN_RING_AREA: i64 = 512;

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
    let chunk_compression = sort_writer.compression();

    struct OceanAcc {
        records: Vec<SortRecord>,
        bytes: usize,
        chunk_paths: Vec<std::path::PathBuf>,
        count: u64,
        simp_scratch: geometry::SimplifyMultiScratch,
        compression: sort::ChunkCompression,
    }

    impl OceanAcc {
        fn flush(&mut self, chunk_dir: &std::path::Path, chunk_id: &AtomicUsize) {
            if self.records.is_empty() {
                return;
            }
            let id = chunk_id.fetch_add(1, Ordering::Relaxed);
            let path = chunk_dir.join(format!("chunk_{id:04}.bin"));
            // Panic: inside rayon fold — can't propagate Result. Disk I/O failure is unrecoverable.
            sort::write_sorted_chunk(&mut self.records, &path, self.compression)
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
            || OceanAcc { records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0, simp_scratch: geometry::SimplifyMultiScratch::new(), compression: chunk_compression },
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
            || OceanAcc { records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0, simp_scratch: geometry::SimplifyMultiScratch::new(), compression: chunk_compression },
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

    // No pre-clip Mercator simplification for ocean.
    // Pre-clip DP on coastlines creates self-intersecting rings (narrow channels
    // collapse), which then produce garbage after S-H clipping. Instead we iterate
    // zooms with the original geometry and simplify in tile coords after clipping
    // (simplify_ring_safe in emit_boundary_tile).
    let _ = simp_scratch; // unused — ocean skips pre-clip simplification
    for z in (min_zoom..=max_zoom).rev() {
        let (simp_outer, simp_inners): (&[Point], &[Vec<Point>]) = (outer, inners);
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
            if row_clip_a.is_empty() { continue; }
            row_outer.clear();
            row_outer.extend_from_slice(&row_clip_a);

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

            if let Some(bx_list) = boundary_rows.get(&ty) {
                // X-extent of row-clipped polygon — skip boundary tiles outside this range
                let (row_x_min, row_x_max) = row_outer.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| (lo.min(p.x), hi.max(p.x)));

                // Row has boundary tiles — clip+emit them, then clip+emit gaps
                for &tx in bx_list {
                    let tile_x_min = f64::from(tx) * inv_scale - tile_buf;
                    let tile_x_max = f64::from(tx + 1) * inv_scale + tile_buf;
                    if row_x_max < tile_x_min || row_x_min > tile_x_max { continue; }
                    if let Some(mask) = land_mask {
                        if !mask.has_land(z, tx, ty) {
                            emit_full_tile(feature_id, tx, ty, z, layer_idx, attrs, records, &mut bt_all_rings, &mut bt_geom_buf);
                            continue;
                        }
                    }
                    emit_boundary_tile(
                        feature_id, tx, ty, z, &row_outer, &row_inners[..row_inner_count],
                        simp_outer, simp_inners,
                        layer_idx, attrs, records,
                        &mut bt_all_rings, &mut bt_geom_buf,
                        &mut clip_a, &mut clip_b,
                    );
                }

                // Gap tiles: use PIP to decide if inside, then clip actual polygon
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
                            if let Some(mask) = land_mask {
                                if !mask.has_land(z, tx, ty) {
                                    emit_full_tile(feature_id, tx, ty, z, layer_idx, attrs, records, &mut bt_all_rings, &mut bt_geom_buf);
                                    continue;
                                }
                            }
                            emit_boundary_tile(
                                feature_id, tx, ty, z, &row_outer, &row_inners[..row_inner_count],
                                simp_outer, simp_inners,
                                layer_idx, attrs, records,
                                &mut bt_all_rings, &mut bt_geom_buf,
                                &mut clip_a, &mut clip_b,
                            );
                        }
                    }
                }
            } else {
                // No boundary tiles in this row — single PIP test, then clip each tile
                let test_cx = (f64::from(tx_min) + 0.5) * inv_scale;
                if pip(test_cx, cy) {
                    for tx in tx_min..=tx_max {
                        if let Some(mask) = land_mask {
                            if !mask.has_land(z, tx, ty) {
                                emit_full_tile(feature_id, tx, ty, z, layer_idx, attrs, records, &mut bt_all_rings, &mut bt_geom_buf);
                                continue;
                            }
                        }
                        emit_boundary_tile(
                            feature_id, tx, ty, z, &row_outer, &row_inners[..row_inner_count],
                            simp_outer, simp_inners,
                            layer_idx, attrs, records,
                            &mut bt_all_rings, &mut bt_geom_buf,
                            &mut clip_a, &mut clip_b,
                        );
                    }
                }
            }
        }
    }
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
    orig_outer: &[Point],       // un-preclipped polygon (for i_overlay fallback)
    orig_inners: &[Vec<Point>], // un-preclipped holes
    layer_idx: u8,
    attrs: &[shortbread::Attr],
    records: &mut Vec<SortRecord>,
    all_rings: &mut Vec<Vec<(i32, i32)>>,
    geom_buf: &mut Vec<u32>,
    clip_a: &mut Vec<Point>,
    clip_b: &mut Vec<Point>,
) {
    let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);

    // S-H clip, then check for bridge edges (concavity fill artifacts).
    // S-H connects exit/re-entry points on the same clip edge with a bridge,
    // covering area that should be separate polygons (e.g. filling in islands).
    // The bridge ring is simple (no crossings) but topologically wrong.
    geometry::clip_polygon_into(outer, &clip, clip_a, clip_b);
    if clip_a.len() < 3 {
        return;
    }
    let has_bridge = has_boundary_bridge(clip_a, &clip);

    let outer_tc;
    let mut hole_tcs: Vec<Vec<(i32, i32)>> = Vec::new();
    if !has_bridge {
        // S-H output has no bridge edges — quantize and clip holes
        outer_tc = geometry::to_tile_coords(clip_a, tx, ty, z);
        for inner in inners {
            geometry::clip_polygon_into(inner, &clip, clip_a, clip_b);
            if clip_a.len() < 3 { continue; }
            let inner_tc = geometry::to_tile_coords(clip_a, tx, ty, z);
            if inner_tc.len() >= 4 {
                hole_tcs.push(inner_tc);
            }
        }
    } else {
        outer_tc = Vec::new(); // dummy — overwritten by fallback
    };

    let repaired = if !has_bridge {
        // S-H produced correct topology — repair quantization artifacts only
        geometry::repair_quantized_polygon(&outer_tc, &hole_tcs)
    } else {
        // S-H produced bridge (concave coastline) — re-clip from original
        // Mercator geometry using i_overlay boolean intersection, then quantize + repair
        let robust_polys = geometry::clip_polygon_robust(orig_outer, orig_inners, &clip);
        let mut all_repaired = Vec::new();
        for poly in robust_polys {
            let otc = geometry::to_tile_coords(&poly.outer, tx, ty, z);
            let htcs: Vec<Vec<(i32, i32)>> = poly.holes.iter()
                .filter(|h| h.len() >= 3)
                .map(|h| geometry::to_tile_coords(h, tx, ty, z))
                .collect();
            all_repaired.extend(geometry::repair_quantized_polygon(&otc, &htcs));
        }
        all_repaired
    };
    if repaired.is_empty() { return; }

    let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
    let key_base = sort::make_sort_key(tile_id, layer_idx, 0);

    // Emit each repaired polygon as a separate feature
    for poly in repaired {
        if poly.is_empty() { continue; }
        all_rings.clear();
        for (i, mut ring) in poly.into_iter().enumerate() {
            if ring.len() < 4 { continue; }
            if ring_area_abs(&ring) < MIN_RING_AREA { continue; }
            if i == 0 {
                close_and_orient_cw(&mut ring);
            } else {
                close_and_orient_ccw(&mut ring);
                geometry::nudge_hole_off_boundary(&mut ring);
            }
            all_rings.push(ring);
        }
        if all_rings.is_empty() { continue; }

        let n_rings = all_rings.len();
        let ring_count = geometry::filter_holes_for_outer(all_rings, n_rings);
        if ring_count > 1 {
            let (outer_ref, holes) = all_rings[..ring_count].split_first_mut().expect("nonempty");
            for hole in holes {
                geometry::nudge_coincident_hole_vertices(hole, outer_ref);
            }
        }

        let ring_refs: Vec<&[(i32, i32)]> = all_rings[..ring_count].iter().map(Vec::as_slice).collect();
        mvt::encode_polygon(geom_buf, &ring_refs);
        if geom_buf.is_empty() { continue; }
        let data = encode_feature_data(feature_id, GeomType::Polygon, geom_buf, attrs, z);
        records.push(SortRecord { key: key_base, data });
    }
}

/// DP-simplify a closed ring, but fall back to the original if simplification
/// creates a self-intersecting result. Ocean coastlines can form narrow channels
/// where DP collapses vertices across the gap, creating crossings.
/// Detect S-H bridge edges: segments where both endpoints lie on the same clip
/// boundary. S-H creates these when a concave polygon exits and re-enters through
/// the same edge, connecting disjoint regions with a zero-width bridge. The bridge
/// ring is simple (no crossings) but covers area it shouldn't (e.g. islands).
/// Detect S-H bridge edges by counting boundary-running segments per clip edge.
/// A single segment on a clip edge is normal (polygon enters/exits at that edge).
/// TWO or more segments on the SAME edge means S-H connected disjoint regions
/// with a bridge — the concavity-fill artifact.
fn has_boundary_bridge(ring: &[Point], clip: &ClipRect) -> bool {
    let eps = 1e-10;
    let mut left = 0u32;
    let mut right = 0u32;
    let mut bottom = 0u32;
    let mut top = 0u32;
    for i in 0..ring.len().saturating_sub(1) {
        let a = ring[i];
        let b = ring[i + 1];
        if (a.x - clip.min_x).abs() < eps && (b.x - clip.min_x).abs() < eps { left += 1; }
        if (a.x - clip.max_x).abs() < eps && (b.x - clip.max_x).abs() < eps { right += 1; }
        if (a.y - clip.min_y).abs() < eps && (b.y - clip.min_y).abs() < eps { bottom += 1; }
        if (a.y - clip.max_y).abs() < eps && (b.y - clip.max_y).abs() < eps { top += 1; }
    }
    left > 1 || right > 1 || bottom > 1 || top > 1
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

fn simplify_ring_safe(ring: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let simplified = geometry::simplify_ring_dp(ring, geometry::TILE_SIMPLIFY_TOLERANCE);
    if geometry::ring_is_simple(&simplified) {
        simplified
    } else {
        ring.to_vec()
    }
}

/// Absolute 2x signed area of a closed ring (shoelace formula, no /2).
fn ring_area_abs(ring: &[(i32, i32)]) -> i64 {
    let mut area: i64 = 0;
    let n = ring.len();
    if n < 3 {
        return 0;
    }
    for i in 0..n - 1 {
        area += (ring[i].0 as i64) * (ring[i + 1].1 as i64)
            - (ring[i + 1].0 as i64) * (ring[i].1 as i64);
    }
    area.abs()
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
            &mut sort_writer,
        )
        .unwrap();
        assert_eq!(emitted, 0, "truncated record should be skipped");
    }
}
