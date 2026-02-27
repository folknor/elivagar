use super::*;

const EPSILON: f64 = 1e-6;

fn approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() < EPSILON
}

// --- Projection tests ---

#[test]
fn test_project_origin() {
    let p = project(0.0, 0.0);
    assert!(approx_eq(p.x, 0.5), "x={}, expected 0.5", p.x);
    assert!(approx_eq(p.y, 0.5), "y={}, expected 0.5", p.y);
}

#[test]
fn test_project_copenhagen() {
    // Copenhagen: ~55.68°N, 12.57°E
    let p = project(55.68, 12.57);
    // x = (12.57 + 180) / 360 ≈ 0.53492
    assert!(approx_eq(p.x, 0.534_917), "x={}", p.x);
    // y should be < 0.5 (northern hemisphere)
    assert!(p.y < 0.5, "y={} should be < 0.5 for northern hemisphere", p.y);
    assert!(p.y > 0.0, "y={} should be > 0.0", p.y);
    // y = 0.5 - ln(tan(lat) + sec(lat)) / (2π) ≈ 0.3130
    assert!(approx_eq(p.y, 0.312_976), "y={}", p.y);
}

#[test]
fn test_project_e7() {
    // Copenhagen in e7: lat=556800000, lon=125700000
    let p = project_e7(556_800_000, 125_700_000);
    let p2 = project(55.68, 12.57);
    assert!(approx_eq(p.x, p2.x), "x: {} vs {}", p.x, p2.x);
    assert!(approx_eq(p.y, p2.y), "y: {} vs {}", p.y, p2.y);
}

#[test]
fn test_project_e7_lut_accuracy() {
    // Sweep 1° steps from -85° to +85°, verify LUT matches exact projection.
    for lat_deg in -85..=85 {
        #[allow(clippy::cast_possible_truncation)]
        let lat_e7 = (lat_deg as f64 * 1e7) as i32;
        let lut_p = project_e7(lat_e7, 0);
        let exact_p = project(lat_deg as f64, 0.0);
        assert!(
            (lut_p.y - exact_p.y).abs() < EPSILON,
            "LUT mismatch at lat={lat_deg}°: lut={}, exact={}, diff={}",
            lut_p.y, exact_p.y, (lut_p.y - exact_p.y).abs(),
        );
    }
}

#[test]
fn test_project_extreme_latitude_clamped() {
    // Beyond ±85.0511 should be clamped
    let p_north = project(90.0, 0.0);
    let p_max = project(MAX_LATITUDE, 0.0);
    assert!(
        approx_eq(p_north.y, p_max.y),
        "90° should clamp to same as {MAX_LATITUDE}°: {} vs {}",
        p_north.y,
        p_max.y,
    );
}

#[test]
fn test_merc_y_to_lat_roundtrip() {
    let lat = 55.68;
    let p = project(lat, 0.0);
    let recovered_lat = merc_y_to_lat(p.y);
    assert!(
        approx_eq(recovered_lat, lat),
        "roundtrip lat: {recovered_lat} vs {lat}",
    );
}

// --- Tile coordinate tests ---

#[test]
fn test_merc_to_tile_px_center() {
    // At zoom 0, the whole world is one tile [0,0].
    // Mercator (0.5, 0.5) → center of the tile → (2048, 2048)
    let (px, py) = merc_to_tile_px(&Point::new(0.5, 0.5), 0, 0, 0);
    assert_eq!(px, 2048);
    assert_eq!(py, 2048);
}

#[test]
fn test_merc_to_tile_px_origin() {
    // Mercator (0.0, 0.0) at zoom 0, tile (0,0) → (0, 0)
    let (px, py) = merc_to_tile_px(&Point::new(0.0, 0.0), 0, 0, 0);
    assert_eq!(px, 0);
    assert_eq!(py, 0);
}

// --- Simplification tests ---

#[test]
fn test_simplify_triangle_preserved() {
    let points = vec![
        Point::new(0.0, 0.0),
        Point::new(0.5, 1.0),
        Point::new(1.0, 0.0),
    ];
    let simplified = simplify(&points, 0.01);
    assert_eq!(simplified.len(), 3, "triangle should be preserved with small tolerance");
}

#[test]
fn test_simplify_triangle_collapsed() {
    let points = vec![
        Point::new(0.0, 0.0),
        Point::new(0.5, 0.001), // very close to the line
        Point::new(1.0, 0.0),
    ];
    let simplified = simplify(&points, 0.01);
    assert_eq!(simplified.len(), 2, "near-collinear point should be removed");
}

#[test]
fn test_simplify_two_points() {
    let points = vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)];
    let simplified = simplify(&points, 0.1);
    assert_eq!(simplified.len(), 2, "two-point line always preserved");
}

#[test]
fn test_simplify_preserves_endpoints() {
    let points = vec![
        Point::new(0.0, 0.0),
        Point::new(0.25, 0.0001),
        Point::new(0.5, 0.0001),
        Point::new(0.75, 0.0001),
        Point::new(1.0, 0.0),
    ];
    let simplified = simplify(&points, 0.01);
    assert!(approx_eq(simplified[0].x, 0.0), "first point preserved");
    assert!(
        approx_eq(simplified[simplified.len() - 1].x, 1.0),
        "last point preserved",
    );
}

// --- Line clipping tests ---

#[test]
fn test_clip_line_crossing() {
    let rect = ClipRect::new(0.0, 0.0, 1.0, 1.0);
    let line = vec![
        Point::new(-0.5, 0.5),
        Point::new(1.5, 0.5),
    ];
    let clipped = clip_linestring(&line, &rect);
    assert_eq!(clipped.len(), 1, "should produce one sub-line");
    let seg = &clipped[0];
    assert_eq!(seg.len(), 2);
    assert!(approx_eq(seg[0].x, 0.0), "entry at left edge: x={}", seg[0].x);
    assert!(approx_eq(seg[1].x, 1.0), "exit at right edge: x={}", seg[1].x);
}

#[test]
fn test_clip_line_fully_inside() {
    let rect = ClipRect::new(0.0, 0.0, 1.0, 1.0);
    let line = vec![
        Point::new(0.2, 0.2),
        Point::new(0.8, 0.8),
    ];
    let clipped = clip_linestring(&line, &rect);
    assert_eq!(clipped.len(), 1);
    assert_eq!(clipped[0].len(), 2);
}

#[test]
fn test_clip_line_fully_outside() {
    let rect = ClipRect::new(0.0, 0.0, 1.0, 1.0);
    let line = vec![
        Point::new(2.0, 2.0),
        Point::new(3.0, 3.0),
    ];
    let clipped = clip_linestring(&line, &rect);
    assert!(clipped.is_empty(), "line fully outside should produce no output");
}

#[test]
fn test_clip_line_multiple_crossings() {
    // Line enters, exits, re-enters the box
    let rect = ClipRect::new(0.0, 0.0, 1.0, 1.0);
    let line = vec![
        Point::new(-0.5, 0.5),
        Point::new(0.5, 0.5),
        Point::new(1.5, 0.5),
        Point::new(2.5, 0.5), // outside
    ];
    let clipped = clip_linestring(&line, &rect);
    // The line enters at x=0, continues to x=0.5 (inside), then exits at x=1.0
    // The segment from 1.5 to 2.5 is fully outside
    assert_eq!(clipped.len(), 1, "should produce one contiguous sub-line");
}

// --- Polygon clipping tests ---

#[test]
fn test_clip_polygon_fully_inside() {
    let rect = ClipRect::new(0.0, 0.0, 1.0, 1.0);
    let ring = vec![
        Point::new(0.2, 0.2),
        Point::new(0.8, 0.2),
        Point::new(0.8, 0.8),
        Point::new(0.2, 0.8),
    ];
    let clipped = clip_polygon(&ring, &rect);
    assert_eq!(clipped.len(), 4, "fully inside polygon unchanged");
}

#[test]
fn test_clip_polygon_partially_outside() {
    let rect = ClipRect::new(0.0, 0.0, 1.0, 1.0);
    // A square that extends beyond the right edge
    let ring = vec![
        Point::new(0.5, 0.2),
        Point::new(1.5, 0.2),
        Point::new(1.5, 0.8),
        Point::new(0.5, 0.8),
    ];
    let clipped = clip_polygon(&ring, &rect);
    // Should be clipped to right edge at x=1.0
    assert!(!clipped.is_empty(), "partially overlapping polygon should produce output");
    for p in &clipped {
        assert!(p.x >= -EPSILON, "x={} should be >= 0", p.x);
        assert!(p.x <= 1.0 + EPSILON, "x={} should be <= 1", p.x);
        assert!(p.y >= -EPSILON, "y={} should be >= 0", p.y);
        assert!(p.y <= 1.0 + EPSILON, "y={} should be <= 1", p.y);
    }
}

#[test]
fn test_clip_polygon_fully_outside() {
    let rect = ClipRect::new(0.0, 0.0, 1.0, 1.0);
    let ring = vec![
        Point::new(2.0, 2.0),
        Point::new(3.0, 2.0),
        Point::new(3.0, 3.0),
        Point::new(2.0, 3.0),
    ];
    let clipped = clip_polygon(&ring, &rect);
    assert!(clipped.is_empty(), "fully outside polygon should be empty");
}

// --- Ring orientation tests ---

#[test]
fn test_ccw_ring() {
    // Counter-clockwise square
    let ring = vec![
        Point::new(0.0, 0.0),
        Point::new(1.0, 0.0),
        Point::new(1.0, 1.0),
        Point::new(0.0, 1.0),
    ];
    assert!(is_ccw(&ring), "CCW ring should be detected as CCW");
    assert!(!is_cw(&ring), "CCW ring should not be detected as CW");
}

#[test]
fn test_cw_ring() {
    // Clockwise square (reversed)
    let ring = vec![
        Point::new(0.0, 1.0),
        Point::new(1.0, 1.0),
        Point::new(1.0, 0.0),
        Point::new(0.0, 0.0),
    ];
    assert!(is_cw(&ring), "CW ring should be detected as CW");
    assert!(!is_ccw(&ring), "CW ring should not be detected as CCW");
}

#[test]
fn test_signed_area_unit_square() {
    // CCW unit square has area +0.5 * ... = +1.0
    let ring = vec![
        Point::new(0.0, 0.0),
        Point::new(1.0, 0.0),
        Point::new(1.0, 1.0),
        Point::new(0.0, 1.0),
    ];
    let area = signed_area(&ring);
    assert!(approx_eq(area, 1.0), "unit square area: {area}, expected 1.0");
}

#[test]
fn test_reverse_ring() {
    let mut ring = vec![
        Point::new(0.0, 0.0),
        Point::new(1.0, 0.0),
        Point::new(1.0, 1.0),
    ];
    assert!(is_ccw(&ring));
    reverse_ring(&mut ring);
    assert!(is_cw(&ring));
}

// --- Tile intersection tests ---

#[test]
fn test_tiles_for_bbox_zoom_0() {
    let bbox = MercBbox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };
    let tiles = tiles_for_bbox(&bbox, 0);
    assert_eq!(tiles.len(), 1);
    assert_eq!(tiles[0], (0, 0));
}

#[test]
fn test_tiles_for_bbox_zoom_1() {
    // Whole world at zoom 1 → 4 tiles
    let bbox = MercBbox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 0.999,
        max_y: 0.999,
    };
    let tiles = tiles_for_bbox(&bbox, 1);
    assert_eq!(tiles.len(), 4);
    assert!(tiles.contains(&(0, 0)));
    assert!(tiles.contains(&(1, 0)));
    assert!(tiles.contains(&(0, 1)));
    assert!(tiles.contains(&(1, 1)));
}

#[test]
fn test_tiles_for_bbox_single_tile() {
    // A small bbox in the upper-left quadrant at zoom 1
    let bbox = MercBbox {
        min_x: 0.1,
        min_y: 0.1,
        max_x: 0.4,
        max_y: 0.4,
    };
    let tiles = tiles_for_bbox(&bbox, 1);
    assert_eq!(tiles.len(), 1);
    assert_eq!(tiles[0], (0, 0));
}

#[test]
fn test_tiles_for_bbox_copenhagen() {
    // Copenhagen at zoom 10
    let bbox = project_bbox(55.6, 12.5, 55.7, 12.6);
    let tiles = tiles_for_bbox(&bbox, 10);
    assert!(!tiles.is_empty(), "Copenhagen should intersect at least one tile");
    // At zoom 10 it should be a small number of tiles
    assert!(tiles.len() <= 4, "should be a small number of tiles: {}", tiles.len());
}

// --- Area tests ---

#[test]
fn test_area_sq_meters_equator() {
    // A 1-degree × 1-degree box at the equator ≈ 111km × 111km ≈ 12321 km²
    let sw = project(0.0, 0.0);
    let se = project(0.0, 1.0);
    let ne = project(1.0, 1.0);
    let nw = project(1.0, 0.0);
    let ring = vec![sw, se, ne, nw];
    let area = area_sq_meters(&ring);
    let area_km2 = area / 1e6;
    // Should be roughly 12,000 km² (not exact due to Mercator approximation)
    assert!(
        area_km2 > 10_000.0 && area_km2 < 15_000.0,
        "1°×1° at equator ≈ 12,000 km², got {area_km2:.0} km²",
    );
}

#[test]
fn test_area_sq_meters_high_latitude() {
    // A 1° × 1° box at 70°N. At 70°N, 1° longitude ≈ 38 km, 1° latitude ≈ 111 km.
    // Expected area ≈ 38 × 111 ≈ 4,218 km².
    let sw = project(70.0, 10.0);
    let se = project(70.0, 11.0);
    let ne = project(71.0, 11.0);
    let nw = project(71.0, 10.0);
    let ring = vec![sw, se, ne, nw];
    let area_km2 = area_sq_meters(&ring) / 1e6;
    assert!(
        area_km2 > 3_500.0 && area_km2 < 5_000.0,
        "1°×1° at 70°N ≈ 4,200 km², got {area_km2:.0} km²",
    );
}

#[test]
fn test_area_sq_meters_wide_latitude_span() {
    // A 10° longitude × 25° latitude box from 55°N to 80°N with vertices
    // at every degree of latitude — simulating a real OSM polygon boundary.
    //
    // Reference area via spherical integration:
    //   A = R² × Δλ × ∫cos(φ)dφ = (C/2π)² × (10°×π/180) × [sin(80°)-sin(55°)]
    //   ≈ 1,175,000 km²
    //
    // With dense vertices, each edge spans ~1° of latitude, so the per-edge
    // midpoint cos² correction is accurate.
    let mut ring = Vec::new();
    // Bottom edge: 55°N, west to east
    ring.push(project(55.0, 20.0));
    ring.push(project(55.0, 30.0));
    // Right edge: 30°E, ascending each degree
    for lat in 56..=80 {
        ring.push(project(lat as f64, 30.0));
    }
    // Top edge: 80°N, east to west
    ring.push(project(80.0, 20.0));
    // Left edge: 20°E, descending each degree
    for lat in (55..80).rev() {
        ring.push(project(lat as f64, 20.0));
    }

    let area_km2 = area_sq_meters(&ring) / 1e6;
    // Allow ±5% from the spherical reference value.
    assert!(
        area_km2 > 1_115_000.0 && area_km2 < 1_235_000.0,
        "10°×25° at 55-80°N ≈ 1,175,000 km², got {area_km2:.0} km²",
    );
}

// --- Point on surface tests ---

#[test]
fn test_point_on_surface_square() {
    let ring = vec![
        Point::new(0.0, 0.0),
        Point::new(1.0, 0.0),
        Point::new(1.0, 1.0),
        Point::new(0.0, 1.0),
    ];
    let p = point_on_surface(&ring).expect("should find a point");
    assert!(p.x > 0.0 && p.x < 1.0, "x={} should be inside", p.x);
    assert!(p.y > 0.0 && p.y < 1.0, "y={} should be inside", p.y);
}

#[test]
fn test_point_on_surface_degenerate() {
    // Too few points
    let ring = vec![Point::new(0.0, 0.0), Point::new(1.0, 0.0)];
    assert!(point_on_surface(&ring).is_none());
}

// --- ClipRect for tile ---

#[test]
fn test_clip_rect_for_tile() {
    let rect = ClipRect::for_tile(0, 0, 1, 0.0);
    assert!(approx_eq(rect.min_x, 0.0), "min_x={}", rect.min_x);
    assert!(approx_eq(rect.min_y, 0.0), "min_y={}", rect.min_y);
    assert!(approx_eq(rect.max_x, 0.5), "max_x={}", rect.max_x);
    assert!(approx_eq(rect.max_y, 0.5), "max_y={}", rect.max_y);
}

#[test]
fn test_clip_rect_for_tile_with_buffer() {
    let rect = ClipRect::for_tile(0, 0, 1, 0.1);
    // Buffer extends by 0.1 * 0.5 = 0.05 on each side
    assert!(rect.min_x < 0.0, "buffered min_x={} should be < 0", rect.min_x);
    assert!(rect.max_x > 0.5, "buffered max_x={} should be > 0.5", rect.max_x);
}

// --- Simplification tolerance ---

#[test]
fn test_simplify_tolerance_decreases_with_zoom() {
    let tol_0 = simplify_tolerance(0);
    let tol_10 = simplify_tolerance(10);
    assert!(
        tol_0 > tol_10,
        "tolerance at z0 ({tol_0}) should be > z10 ({tol_10})",
    );
}

// --- Pre-DP subpixel check ---

#[test]
fn test_merc_bbox_subpixel_tiny_feature() {
    // A feature smaller than 1 pixel at z10 should be subpixel.
    // 1 pixel at z10 = 1 / (256 × 1024) ≈ 3.8e-6 Mercator units.
    let pixel_z10 = 1.0 / (256.0 * 1024.0);
    let tiny = vec![
        Point::new(0.5, 0.5),
        Point::new(0.5 + pixel_z10 * 0.1, 0.5 + pixel_z10 * 0.1),
    ];
    assert!(merc_bbox_is_subpixel(&tiny, 10));
    // Same feature should NOT be subpixel at z14 (pixel is 16× smaller).
    assert!(!merc_bbox_is_subpixel(&tiny, 14));
}

#[test]
fn test_merc_bbox_subpixel_large_feature() {
    // A feature spanning 0.01 Mercator units is visible at all zooms.
    let large = vec![
        Point::new(0.5, 0.5),
        Point::new(0.51, 0.51),
    ];
    for z in 0..=14 {
        assert!(!merc_bbox_is_subpixel(&large, z));
    }
}

// --- LandMask tests ---

#[test]
fn test_land_mask_mark_and_query_z8() {
    let mask = LandMask::new();
    assert!(!mask.has_land(8, 100, 100));
    // mark_bbox uses Mercator [0,1] coords → z14 tile 100/256*16384 = 6400
    mask.mark_bbox(&MercBbox {
        min_x: 100.0 / 256.0,
        min_y: 100.0 / 256.0,
        max_x: 100.5 / 256.0,
        max_y: 100.5 / 256.0,
    });
    // z8 tile (100,100) has z14 descendants (6400..6463, 6400..6463)
    assert!(mask.has_land(8, 100, 100));
    assert!(!mask.has_land(8, 101, 100));
}

#[test]
fn test_land_mask_z14_ancestor() {
    let mask = LandMask::new();
    // Set z14 cell (6400, 6400) directly
    mask.set_bit(6400, 6400);
    assert!(mask.has_land(14, 6400, 6400));
    assert!(!mask.has_land(14, 6401, 6401)); // different z14 cell
    // z8 tile (100, 100) has z14 descendant (6400, 6400) set
    assert!(mask.has_land(8, 100, 100));
    // Different z8 cell → no z14 descendants set
    assert!(!mask.has_land(8, 101, 101));
}

#[test]
fn test_land_mask_low_zoom_descendant() {
    let mask = LandMask::new();
    // Set z14 cell (6400, 6400)
    mask.set_bit(6400, 6400);
    // z7 tile (50, 50) covers z14 cells (6400..6527, 6400..6527)
    assert!(mask.has_land(7, 50, 50));
    // z7 tile (51, 50) covers z14 cells (6528..6655, 6400..6527)
    assert!(!mask.has_land(7, 51, 50));
    // z0 tile (0, 0) covers all z14 cells
    assert!(mask.has_land(0, 0, 0));
}

#[test]
fn test_land_mask_serialization_roundtrip() {
    let mask = LandMask::new();
    mask.set_bit(0, 0);
    mask.set_bit(16383, 16383);
    mask.set_bit(6400, 3200);
    let bytes = mask.to_bytes();
    assert_eq!(bytes.len(), LandMask::BYTES);
    let restored = LandMask::from_bytes(&bytes).expect("deserialization failed");
    assert!(restored.get_bit(0, 0));
    assert!(restored.get_bit(16383, 16383));
    assert!(restored.get_bit(6400, 3200));
    assert!(!restored.get_bit(1, 0));
    assert_eq!(restored.count_set(), 3);
}

#[test]
fn test_land_mask_count() {
    let mask = LandMask::new();
    assert_eq!(mask.count_set(), 0);
    mask.set_bit(10, 20);
    mask.set_bit(10, 21);
    assert_eq!(mask.count_set(), 2);
    // Duplicate set doesn't change count
    mask.set_bit(10, 20);
    assert_eq!(mask.count_set(), 2);
}

#[test]
fn test_land_mask_from_bytes_wrong_length() {
    assert!(LandMask::from_bytes(&[0; 100]).is_none());
    assert!(LandMask::from_bytes(&[]).is_none());
}
