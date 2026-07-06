// Ocean shapefile processing.
//
// Reads a water-polygons-split-3857 shapefile (mmap + .shx index), filters to
// data bounds, parses polygons, and processes them in parallel with rayon.
// Uses scanline fill to minimize point-in-polygon tests.

use crate::geometry::{self, MercBbox, Point};
use crate::geometry::int_ocean::{
    IntEmitScratch, IntRect, OCEAN_DP_TOL_PX, Shape, ZoomEmitParams, emit_shape_for_zoom,
    intersect_rect, quantize_polygon, shape_bbox,
};
use crate::mvt::GeomType;
use crate::pmtiles_writer;
use crate::shortbread::{self, Layer};
use crate::sort::{self, SortRecord, SortWriter};
use crate::wire_format::encode_feature_data;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

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
            // Count ALL rings: hole-heavy pieces must not dodge the split.
            let total_vertices: usize = piece.iter().map(Vec::len).sum();
            if total_vertices < SPLIT_MIN_VERTICES {
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
                    // Split tile fully containing the piece: no cut needed.
                    if rect_contains(tile_rect, bb) {
                        split_out.push(piece.clone());
                        continue;
                    }
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
    // The common case for an in-bounds extract: the shape lies entirely
    // inside the data bounds - the boolean is an expensive identity.
    if shape_bbox(&shape).is_some_and(|bb| rect_contains(data_rect, bb)) {
        pieces.push(shape);
        return;
    }
    pieces.extend(intersect_rect(&shape, data_rect, 0));
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

#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
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

    let mut scratch = IntEmitScratch::new();
    for z in (min_zoom..=max_zoom).rev() {
        let mut sink = |tx: u32, ty: u32, geom: &[u32]| {
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            let key = sort::make_sort_key(tile_id, layer_idx, 0);
            let data = encode_feature_data(feature_id, GeomType::Polygon, geom, attrs, z);
            records.push(SortRecord { key, data });
        };
        emit_shape_for_zoom(
            piece,
            ZoomEmitParams {
                z,
                maxz: max_zoom,
                dp_tol: OCEAN_DP_TOL_PX,
                min_area: 256,
                pins: None,
            },
            &mut scratch,
            &mut sink,
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
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
