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

#[test]
fn test_simplify_with_required_preserves_pinned_vertex() {
    let points = vec![
        Point::new(0.0, 0.0),
        Point::new(0.25, 0.0001),
        Point::new(0.5, 0.0001),
        Point::new(0.75, 0.0001),
        Point::new(1.0, 0.0),
    ];
    let mut keep = Vec::new();
    let mut out = Vec::new();
    let _ = simplify_into_with_required(&points, 0.01, &[2], &mut keep, &mut out);
    assert!(
        out.iter().any(|p| approx_eq(p.x, 0.5) && approx_eq(p.y, 0.0001)),
        "required interior point should survive DP",
    );
}

#[test]
fn test_simplify_with_required_ignores_out_of_range_indices() {
    let points = vec![
        Point::new(0.0, 0.0),
        Point::new(0.5, 0.0),
        Point::new(1.0, 0.0),
    ];
    let mut keep = Vec::new();
    let mut out = Vec::new();
    let _ = simplify_into_with_required(&points, 0.01, &[999], &mut keep, &mut out);
    assert_eq!(out.len(), 2, "invalid required index must be ignored");
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

#[test]
fn test_point_on_surface_with_holes_avoids_hole() {
    let outer = vec![
        Point::new(0.0, 0.0),
        Point::new(10.0, 0.0),
        Point::new(10.0, 10.0),
        Point::new(0.0, 10.0),
    ];
    let hole = vec![
        Point::new(2.0, 2.0),
        Point::new(8.0, 2.0),
        Point::new(8.0, 8.0),
        Point::new(2.0, 8.0),
    ];
    let p = point_on_surface_with_holes(&outer, std::slice::from_ref(&hole)).expect("should find a point");
    assert!(point_in_polygon(&p, &outer));
    assert!(!point_in_polygon(&p, &hole));
}

#[test]
fn test_point_on_surface_with_holes_multiple_holes() {
    let outer = vec![
        Point::new(0.0, 0.0),
        Point::new(10.0, 0.0),
        Point::new(10.0, 10.0),
        Point::new(0.0, 10.0),
    ];
    let hole_a = vec![
        Point::new(1.0, 1.0),
        Point::new(4.5, 1.0),
        Point::new(4.5, 6.0),
        Point::new(1.0, 6.0),
    ];
    let hole_b = vec![
        Point::new(5.5, 4.0),
        Point::new(9.0, 4.0),
        Point::new(9.0, 9.0),
        Point::new(5.5, 9.0),
    ];
    let inners = vec![hole_a.clone(), hole_b.clone()];

    let p = point_on_surface_with_holes(&outer, &inners).expect("should find a point");
    assert!(point_in_polygon(&p, &outer));
    assert!(!point_in_polygon(&p, &hole_a));
    assert!(!point_in_polygon(&p, &hole_b));
}

#[test]
fn test_point_on_surface_with_holes_adjacent_holes() {
    let outer = vec![
        Point::new(0.0, 0.0),
        Point::new(10.0, 0.0),
        Point::new(10.0, 10.0),
        Point::new(0.0, 10.0),
    ];
    // Two holes sharing an edge at x=5.0 (adjacent, no gap).
    let left_hole = vec![
        Point::new(2.0, 2.0),
        Point::new(5.0, 2.0),
        Point::new(5.0, 8.0),
        Point::new(2.0, 8.0),
    ];
    let right_hole = vec![
        Point::new(5.0, 2.0),
        Point::new(8.0, 2.0),
        Point::new(8.0, 8.0),
        Point::new(5.0, 8.0),
    ];
    let inners = vec![left_hole.clone(), right_hole.clone()];

    let p = point_on_surface_with_holes(&outer, &inners).expect("should find a point");
    assert!(point_in_polygon(&p, &outer));
    assert!(!point_in_polygon(&p, &left_hole));
    assert!(!point_in_polygon(&p, &right_hole));
}

#[test]
fn test_point_on_surface_with_holes_fallback_inside_hole_returns_none() {
    let outer = vec![
        Point::new(0.0, 0.0),
        Point::new(10.0, 0.0),
        Point::new(10.0, 10.0),
        Point::new(0.0, 10.0),
    ];
    // Deliberately degenerate for robustness testing: hole equals outer ring.
    // All scan candidates are removed and fallback center lies inside the hole.
    let hole = outer.clone();
    let inners = vec![hole];
    assert!(point_on_surface_with_holes(&outer, &inners).is_none());
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

// --- Interior tile detection tests ---

#[test]
fn tile_is_interior_inside_large_square() {
    // Large square [0.1, 0.1] to [0.9, 0.9]. Tile (1, 1) at z=2 spans
    // [0.25, 0.25] to [0.5, 0.5] — clearly inside the square.
    let ring = vec![
        Point::new(0.1, 0.1),
        Point::new(0.9, 0.1),
        Point::new(0.9, 0.9),
        Point::new(0.1, 0.9),
    ];
    let clip = ClipRect::for_tile(1, 1, 2, BUFFER_FRACTION);
    assert!(tile_is_interior(&ring, &clip));
}

#[test]
fn tile_is_interior_boundary_tile() {
    // Same square, tile (0, 0) at z=2 spans [0, 0] to [0.25, 0.25].
    // The polygon edge at x=0.1, y=0.1 crosses this tile.
    let ring = vec![
        Point::new(0.1, 0.1),
        Point::new(0.9, 0.1),
        Point::new(0.9, 0.9),
        Point::new(0.1, 0.9),
    ];
    let clip = ClipRect::for_tile(0, 0, 2, BUFFER_FRACTION);
    assert!(!tile_is_interior(&ring, &clip));
}

#[test]
fn tile_is_interior_outside_tile() {
    // Small square [0.1, 0.1] to [0.3, 0.3]. Tile (3, 3) at z=2 spans
    // [0.75, 0.75] to [1.0, 1.0] — completely outside.
    let ring = vec![
        Point::new(0.1, 0.1),
        Point::new(0.3, 0.1),
        Point::new(0.3, 0.3),
        Point::new(0.1, 0.3),
    ];
    let clip = ClipRect::for_tile(3, 3, 2, BUFFER_FRACTION);
    assert!(!tile_is_interior(&ring, &clip));
}

#[test]
fn tile_is_interior_concave_polygon() {
    // L-shaped polygon: tile in the concave cutout should return false.
    let ring = vec![
        Point::new(0.1, 0.1),
        Point::new(0.9, 0.1),
        Point::new(0.9, 0.5),
        Point::new(0.5, 0.5),
        Point::new(0.5, 0.9),
        Point::new(0.1, 0.9),
    ];
    // Tile (3, 3) at z=2: [0.75, 0.75] to [1.0, 1.0] — inside the cutout.
    let clip = ClipRect::for_tile(3, 3, 2, BUFFER_FRACTION);
    assert!(!tile_is_interior(&ring, &clip));
}

// ---------------------------------------------------------------------------
// for_each_zoom_simplified_multi tests
// ---------------------------------------------------------------------------

/// Helper: make a square polygon ring centered at (cx, cy) with half-width hw.
fn square_ring(cx: f64, cy: f64, hw: f64) -> Vec<Point> {
    vec![
        Point::new(cx - hw, cy - hw),
        Point::new(cx + hw, cy - hw),
        Point::new(cx + hw, cy + hw),
        Point::new(cx - hw, cy + hw),
        Point::new(cx - hw, cy - hw),
    ]
}

#[test]
fn multi_simplify_no_inners_all_zooms() {
    // Large outer ring at z14 only — no simplification needed.
    let outer = square_ring(0.5, 0.5, 0.1);
    let inners: Vec<Vec<Point>> = vec![];
    let mut scratch = SimplifyMultiScratch::new();
    let mut results: Vec<(u8, usize, usize)> = Vec::new();
    for_each_zoom_simplified_multi(&outer, &inners, 14, 14, &mut scratch, |z, o, i| {
        results.push((z, o.len(), i.len()));
    });
    assert_eq!(results.len(), 1);
    assert_eq!(results[0], (14, 5, 0)); // 5-point square, 0 inners
}

#[test]
fn multi_simplify_callback_per_zoom() {
    // Outer large enough to survive multiple zoom levels.
    let outer = square_ring(0.5, 0.5, 0.1);
    let inners: Vec<Vec<Point>> = vec![];
    let mut scratch = SimplifyMultiScratch::new();
    let mut zooms: Vec<u8> = Vec::new();
    for_each_zoom_simplified_multi(&outer, &inners, 10, 14, &mut scratch, |z, _o, _i| {
        zooms.push(z);
    });
    // Should iterate z14, z13, z12, z11, z10 (high to low)
    assert_eq!(zooms, vec![14, 13, 12, 11, 10]);
}

#[test]
fn multi_simplify_inner_count_non_increasing() {
    // Inner count should never increase as zoom decreases (inners can only be
    // dropped, never added). Use a detailed inner with collinear points that
    // will simplify away at low zoom.
    let outer = square_ring(0.5, 0.5, 0.2);
    // Inner: elongated sliver with many collinear-ish points.
    let inner = vec![
        Point::new(0.49, 0.50),
        Point::new(0.495, 0.500_001),
        Point::new(0.50, 0.500_002),
        Point::new(0.505, 0.500_001),
        Point::new(0.51, 0.50),
        Point::new(0.505, 0.499_999),
        Point::new(0.50, 0.499_998),
        Point::new(0.495, 0.499_999),
        Point::new(0.49, 0.50),
    ];
    let inners = vec![inner];
    let mut scratch = SimplifyMultiScratch::new();
    let mut inner_counts: Vec<(u8, usize)> = Vec::new();
    for_each_zoom_simplified_multi(&outer, &inners, 4, 14, &mut scratch, |z, _o, i| {
        inner_counts.push((z, i.len()));
    });
    // At z14, inner should be present
    assert_eq!(inner_counts[0], (14, 1));
    // Inner count should be monotonically non-increasing
    for w in inner_counts.windows(2) {
        assert!(w[0].1 >= w[1].1, "inner count increased from z{} ({}) to z{} ({})",
            w[0].0, w[0].1, w[1].0, w[1].1);
    }
}

#[test]
fn multi_simplify_subpixel_outer_stops_early() {
    // Outer is tiny — should become subpixel and stop iterating before z0.
    let outer = square_ring(0.5, 0.5, 0.00001); // ~1 meter
    let inners: Vec<Vec<Point>> = vec![];
    let mut scratch = SimplifyMultiScratch::new();
    let mut zoom_count = 0;
    for_each_zoom_simplified_multi(&outer, &inners, 0, 14, &mut scratch, |_z, _o, _i| {
        zoom_count += 1;
    });
    // Should NOT reach all 15 zooms — subpixel check should bail out early
    assert!(zoom_count < 15, "subpixel outer should stop early, got {zoom_count} zooms");
}

#[test]
fn multi_simplify_z14_preserves_all_inners() {
    // At z14 (no simplification), all inners should be present unchanged.
    let outer = square_ring(0.5, 0.5, 0.3);
    let inner1 = square_ring(0.3, 0.5, 0.05);
    let inner2 = square_ring(0.7, 0.5, 0.02);
    let inners = vec![inner1.clone(), inner2.clone()];
    let mut scratch = SimplifyMultiScratch::new();
    let mut z14_data: Option<(Vec<Point>, Vec<Vec<Point>>)> = None;
    for_each_zoom_simplified_multi(&outer, &inners, 14, 14, &mut scratch, |_z, o, i| {
        z14_data = Some((o.to_vec(), i.to_vec()));
    });
    let (out_outer, out_inners) = z14_data.expect("should have z14 callback");
    assert_eq!(out_outer.len(), outer.len(), "outer should be unchanged at z14");
    assert_eq!(out_inners.len(), 2, "both inners should be present at z14");
    assert_eq!(out_inners[0].len(), inner1.len(), "inner1 unchanged at z14");
    assert_eq!(out_inners[1].len(), inner2.len(), "inner2 unchanged at z14");
}

#[test]
fn multi_simplify_outer_vertex_count_non_increasing() {
    // Outer vertex count should never increase as zoom decreases.
    let outer = square_ring(0.5, 0.5, 0.1);
    let inners: Vec<Vec<Point>> = vec![];
    let mut scratch = SimplifyMultiScratch::new();
    let mut vertex_counts: Vec<(u8, usize)> = Vec::new();
    for_each_zoom_simplified_multi(&outer, &inners, 4, 14, &mut scratch, |z, o, _i| {
        vertex_counts.push((z, o.len()));
    });
    // Vertex count should be monotonically non-increasing
    for w in vertex_counts.windows(2) {
        assert!(w[0].1 >= w[1].1, "vertex count increased from z{} ({}) to z{} ({})",
            w[0].0, w[0].1, w[1].0, w[1].1);
    }
    // At z14, should have original 5 vertices (no simplification at z14)
    assert_eq!(vertex_counts[0], (14, 5));
}

// ---------------------------------------------------------------------------
// Buffer constant regression tests (tile seam fix 2026-03-06)
// ---------------------------------------------------------------------------

#[test]
fn buffer_fraction_is_8_rendered_pixels() {
    // BUFFER_FRACTION must be 8 rendered pixels / 256 pixels per tile = 0.03125.
    // A previous bug had 8.0 / 4096.0 (= 0.001953125), which is 8 *extent units*
    // — only 0.5 rendered pixels — causing visible tile seams everywhere.
    assert!((BUFFER_FRACTION - 8.0 / 256.0).abs() < f64::EPSILON);
    assert!((BUFFER_FRACTION - 0.03125).abs() < f64::EPSILON);
}

#[test]
fn buffer_fraction_produces_128_extent_unit_buffer() {
    // 8 rendered pixels × 16 extent units per pixel = 128 extent units of buffer.
    // This is the standard MVT buffer size used by Planetiler, Tippecanoe, etc.
    let buffer_extent_units = BUFFER_FRACTION * EXTENT;
    assert!((buffer_extent_units - 128.0).abs() < f64::EPSILON);
}

#[test]
fn clip_rect_for_tile_extends_by_buffer() {
    // At z=1, each tile spans 0.5 in Mercator space.
    // Buffer = BUFFER_FRACTION / 2^1 = 0.03125 / 2 = 0.015625.
    let clip = ClipRect::for_tile(0, 0, 1, BUFFER_FRACTION);
    let buf = BUFFER_FRACTION / 2.0;
    let eps = 1e-12;
    assert!((clip.min_x - (-buf)).abs() < eps);
    assert!((clip.min_y - (-buf)).abs() < eps);
    assert!((clip.max_x - (0.5 + buf)).abs() < eps);
    assert!((clip.max_y - (0.5 + buf)).abs() < eps);
}

// ---------------------------------------------------------------------------
// Shared chain detection tests
// ---------------------------------------------------------------------------

/// Two squares sharing one edge: A=[0,0]-[10,0]-[10,10]-[0,10]-[0,0] and
/// B=[10,0]-[20,0]-[20,10]-[10,10]-[10,0]. Shared edge: (10,0)→(10,10) in A,
/// (10,10)→(10,0) in B (opposite winding).
#[test]
fn shared_chain_two_adjacent_squares() {
    let ring_a = vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
    let ring_b = vec![(10, 0), (20, 0), (20, 10), (10, 10), (10, 0)];
    let chains = detect_shared_chains(&[ring_a, ring_b]);
    assert_eq!(chains.len(), 1, "expected one shared chain");
    let chain = &chains[0];
    assert_eq!(chain.vertices.len(), 2, "single shared edge = 2 vertices");
    assert_eq!(chain.incidents.len(), 2);
    // One incident is ring 0, the other ring 1.
    let ring_idxs: Vec<usize> = chain.incidents.iter().map(|c| c.ring_idx).collect();
    assert!(ring_idxs.contains(&0));
    assert!(ring_idxs.contains(&1));
}

/// Two squares sharing two consecutive edges (L-shape contact).
/// A=[0,0]-[10,0]-[10,5]-[10,10]-[0,10]-[0,0]
/// B=[10,0]-[20,0]-[20,10]-[10,10]-[10,5]-[10,0]
/// Shared edges: (10,0)→(10,5) and (10,5)→(10,10) → one chain of 3 vertices.
#[test]
fn shared_chain_two_edges_form_one_chain() {
    let ring_a = vec![(0, 0), (10, 0), (10, 5), (10, 10), (0, 10), (0, 0)];
    let ring_b = vec![(10, 0), (20, 0), (20, 10), (10, 10), (10, 5), (10, 0)];
    let chains = detect_shared_chains(&[ring_a, ring_b]);
    assert_eq!(chains.len(), 1, "two consecutive shared edges = one chain");
    assert_eq!(chains[0].vertices.len(), 3, "chain should have 3 vertices");
}

/// No shared edges between non-touching polygons.
#[test]
fn shared_chain_no_shared_edges() {
    let ring_a = vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
    let ring_b = vec![(20, 0), (30, 0), (30, 10), (20, 10), (20, 0)];
    let chains = detect_shared_chains(&[ring_a, ring_b]);
    assert!(chains.is_empty());
}

/// Shared vertex but no shared edge — should return empty.
#[test]
fn shared_chain_shared_vertex_no_shared_edge() {
    // Two triangles touching at a single point (10,10).
    let ring_a = vec![(0, 0), (10, 10), (0, 10), (0, 0)];
    let ring_b = vec![(10, 10), (20, 0), (20, 10), (10, 10)];
    let chains = detect_shared_chains(&[ring_a, ring_b]);
    assert!(chains.is_empty(), "shared vertex alone should not produce a chain");
}

/// Three polygons meeting at a triple point. Each adjacent pair shares one edge.
#[test]
fn shared_chain_triple_junction() {
    // Three triangles meeting at (5, 5):
    // A: (0,0)-(10,0)-(5,5)-(0,0)
    // B: (10,0)-(10,10)-(5,5)-(10,0)
    // C: (0,0)-(5,5)-(0,10)-(0,0)  -- note: shares (0,0)-(5,5) with A, shares (5,5) with B
    // But only A-B share the edge (10,0)-(5,5) and A-C share the edge (0,0)-(5,5).
    let ring_a = vec![(0, 0), (10, 0), (5, 5), (0, 0)];
    let ring_b = vec![(10, 0), (10, 10), (5, 5), (10, 0)];
    let ring_c = vec![(0, 0), (5, 5), (0, 10), (0, 0)];
    let chains = detect_shared_chains(&[ring_a, ring_b, ring_c]);
    // A-B share edge (10,0)-(5,5), A-C share edge (0,0)-(5,5)
    assert_eq!(chains.len(), 2, "two pairs sharing one edge each");
    for chain in &chains {
        assert_eq!(chain.vertices.len(), 2);
        assert_eq!(chain.incidents.len(), 2);
    }
}

/// Single ring — no shared edges possible.
#[test]
fn shared_chain_single_ring() {
    let ring = vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
    let chains = detect_shared_chains(&[ring]);
    assert!(chains.is_empty());
}

/// Empty input.
#[test]
fn shared_chain_empty_input() {
    let chains = detect_shared_chains(&[]);
    assert!(chains.is_empty());
}

/// Degenerate ring with < 2 points.
#[test]
fn shared_chain_degenerate_ring() {
    let ring_a = vec![(0, 0)];
    let ring_b = vec![(0, 0), (10, 0), (10, 10), (0, 0)];
    let chains = detect_shared_chains(&[ring_a, ring_b]);
    assert!(chains.is_empty());
}

/// Opposite winding: the chain should mark one incident as reversed.
#[test]
fn shared_chain_marks_reversed_incident() {
    // A walks edge (10,0)→(10,10), B walks (10,10)→(10,0).
    let ring_a = vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
    let ring_b = vec![(10, 0), (20, 0), (20, 10), (10, 10), (10, 0)];
    let chains = detect_shared_chains(&[ring_a, ring_b]);
    assert_eq!(chains.len(), 1);
    let chain = &chains[0];
    // One incident should be reversed, the other not.
    let reversed_count = chain.incidents.iter().filter(|c| c.reversed).count();
    assert_eq!(reversed_count, 1, "one of two incidents should be reversed");
}

/// Three or more rings sharing the same edge (coincident geometry).
/// Should produce incidents with >2 entries or multiple chains.
#[test]
fn shared_chain_three_rings_same_edge() {
    let ring_a = vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
    let ring_b = vec![(10, 0), (20, 0), (20, 10), (10, 10), (10, 0)];
    // Ring C is a duplicate of ring B (coincident geometry).
    let ring_c = vec![(10, 0), (20, 0), (20, 10), (10, 10), (10, 0)];
    let chains = detect_shared_chains(&[ring_a, ring_b, ring_c]);
    // Should detect shared edges between A-B and A-C (and possibly B-C).
    assert!(!chains.is_empty(), "coincident geometry should produce chains");
}

/// Chain that wraps around the ring start/end point.
/// Ring A: shared edges are the last edge (D→A) and the first edge (A→B),
/// which are consecutive in the ring but cross the start/end seam.
#[test]
fn shared_chain_wrap_around_ring_seam() {
    // Ring A: [A, B, C, D, A] where A=(0,0), B=(10,0), C=(10,10), D=(0,10)
    // Ring B: [A, D, E, F, B, A] where E=(-10,10), F=(-10,0)
    // Shared edges: D→A (edge 3 in ring A, edge 0 in B) and A→B (edge 0 in ring A, edge 4 in B).
    // These are consecutive in ring A (wrapping from edge 3 to edge 0).
    let ring_a = vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
    let ring_b = vec![(0, 0), (0, 10), (-10, 10), (-10, 0), (10, 0), (0, 0)];
    // Ring B edges: 0:(0,0)→(0,10), 1:(0,10)→(-10,10), 2:(-10,10)→(-10,0), 3:(-10,0)→(10,0), 4:(10,0)→(0,0)
    // B edge 0 matches A edge 3 reversed, B edge 4 matches A edge 0 reversed.
    // In A: edges 3,0 are consecutive (wrapping). In B walking backward: 0→4, also consecutive.
    // Ring B edges: 0:(0,0)→(0,10), 1:(0,10)→(-10,10), 2:(-10,10)→(-10,0), 3:(-10,0)→(10,0), 4:(10,0)→(0,0)
    // B edge 0 matches A edge 3 reversed, B edge 4 matches A edge 0 reversed.
    // Chain growth from seed (A edge 0) wraps forward: edge 0 → edge 1 (not shared, stops).
    // But the chain also grows because seed ordering is deterministic: edge 0 of ring A
    // is seeded first and grows. In ring A forward from edge 0: edge 1 is NOT shared,
    // so chain is just edge 0. Then edge 3 of ring A seeds separately.
    // Result: two single-edge chains covering the full shared boundary.
    let chains = detect_shared_chains(&[ring_a, ring_b]);
    // Chain growth initially produces two fragments (edges 0 and 3 of ring A),
    // but merge_seam_chains stitches them into one chain since one's tail
    // connects to the other's head.
    assert_eq!(chains.len(), 1, "seam fragments should be merged into one chain");
    assert_eq!(chains[0].vertices.len(), 3, "two edges = three vertices");
    assert_eq!(chains[0].incidents.len(), 2);
}

/// Chain that IS consecutive in both rings across the seam.
#[test]
fn shared_chain_consecutive_wrap_around() {
    // Ring A: [P0, P1, P2, P3, P0] — a square
    // Ring B shares the last two edges of A: P2→P3 and P3→P0.
    // In ring B these are consecutive (but in reverse direction).
    let ring_a = vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
    // Ring B: [P0, P3, P4, P5, P0] where P4=(-10,10), P5=(-10,0)
    // Wait — that doesn't share P2→P3. Let me construct it properly.
    // Ring A edges: 0:(0,0)→(10,0), 1:(10,0)→(10,10), 2:(10,10)→(0,10), 3:(0,10)→(0,0)
    // Ring B should share edges 2 and 3 of ring A.
    // Edge 2: (10,10)→(0,10) — B needs (0,10)→(10,10)
    // Edge 3: (0,10)→(0,0) — B needs (0,0)→(0,10)
    // Ring B: [(0,0), (0,10), (10,10), (20,10), (20,0), (0,0)]
    // B edges: 0:(0,0)→(0,10), 1:(0,10)→(10,10), 2:(10,10)→(20,10), 3:(20,10)→(20,0), 4:(20,0)→(0,0)
    // B edge 0 matches A edge 3 reversed, B edge 1 matches A edge 2 reversed.
    // In A: edges 2,3 are consecutive. In B: edges 0,1 are consecutive. Should form one chain.
    let ring_b = vec![(0, 0), (0, 10), (10, 10), (20, 10), (20, 0), (0, 0)];
    let chains = detect_shared_chains(&[ring_a, ring_b]);
    assert_eq!(chains.len(), 1, "consecutive shared edges in both rings should form one chain");
    assert_eq!(chains[0].vertices.len(), 3, "two edges = three vertices");
}

// ---------------------------------------------------------------------------
// MVT polygon decoder tests
// ---------------------------------------------------------------------------

/// Round-trip: encode a single ring polygon and decode it back.
#[test]
fn decode_mvt_polygon_single_ring_round_trip() {
    let ring = vec![(100, 200), (300, 200), (300, 400), (100, 400), (100, 200)];
    let mut buf = Vec::new();
    crate::mvt::encode_polygon(&mut buf, &[&ring]);
    let decoded = super::decode_mvt_polygon(&buf);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0], ring);
}

/// Round-trip: multi-ring polygon (outer + inner hole).
#[test]
fn decode_mvt_polygon_multi_ring_round_trip() {
    let outer = vec![(0, 0), (4096, 0), (4096, 4096), (0, 4096), (0, 0)];
    let inner = vec![(1000, 1000), (1000, 3000), (3000, 3000), (3000, 1000), (1000, 1000)];
    let mut buf = Vec::new();
    crate::mvt::encode_polygon(&mut buf, &[&outer, &inner]);
    let decoded = super::decode_mvt_polygon(&buf);
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0], outer);
    assert_eq!(decoded[1], inner);
}

/// Empty command buffer produces no rings.
#[test]
fn decode_mvt_polygon_empty() {
    let decoded = super::decode_mvt_polygon(&[]);
    assert!(decoded.is_empty());
}

/// Round-trip with negative coordinates (buffer region outside tile).
#[test]
fn decode_mvt_polygon_negative_coords() {
    let ring = vec![(-128, -128), (4224, -128), (4224, 4224), (-128, 4224), (-128, -128)];
    let mut buf = Vec::new();
    crate::mvt::encode_polygon(&mut buf, &[&ring]);
    let decoded = super::decode_mvt_polygon(&buf);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0], ring);
}

/// Round-trip: two separate polygons encoded sequentially (as multipolygon).
#[test]
fn decode_mvt_polygon_two_outer_rings() {
    let ring_a = vec![(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)];
    let ring_b = vec![(200, 200), (300, 200), (300, 300), (200, 300), (200, 200)];
    let mut buf = Vec::new();
    crate::mvt::encode_polygon(&mut buf, &[&ring_a, &ring_b]);
    let decoded = super::decode_mvt_polygon(&buf);
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0], ring_a);
    assert_eq!(decoded[1], ring_b);
}

// ---------------------------------------------------------------------------
// Chain canonicalization tests
// ---------------------------------------------------------------------------

/// Two adjacent squares sharing edge (10,0)→(10,10). After canonicalization,
/// both rings have identical vertices along the shared edge.
#[test]
fn canonicalize_two_adjacent_squares() {
    let ring_a = vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
    let ring_b = vec![(10, 0), (20, 0), (20, 10), (10, 10), (10, 0)];
    let chains = super::detect_shared_chains(&[ring_a.clone(), ring_b.clone()]);
    assert_eq!(chains.len(), 1);
    let result = super::canonicalize_shared_chains(&mut [ring_a.clone(), ring_b.clone()], &chains);
    assert_eq!(result.reconciled, 1);
    assert_eq!(result.skipped, 0);

    // Modify ring_b's shared edge to simulate divergence, then canonicalize.
    let mut ring_b = ring_b;
    ring_b[3] = (10, 11); // perturb (10,10) in ring B
    let mut rings = [ring_a, ring_b];
    let result = super::canonicalize_shared_chains(&mut rings, &chains);
    assert_eq!(result.reconciled, 1);
    // After canonicalization, ring B's shared segment should match ring A's.
    // The shared chain vertices are [(10,0), (10,10)] from ring A.
    // Ring B incident is reversed, so (10,10) maps to ring_b[3] and (10,0) maps to ring_b[0].
    assert_eq!(rings[1][3], (10, 10), "shared vertex should be restored");
}

/// Chains with >2 incidents are skipped.
#[test]
fn canonicalize_skips_gt2_incidents() {
    // Three rings sharing the same edge — detect_shared_chains produces
    // chains with 2 incidents each (one per ring pair), but let's test
    // that if we manually construct a 3-incident chain, it gets skipped.
    let chain = super::SharedChain {
        vertices: vec![(0, 0), (10, 0)],
        incidents: vec![
            super::ChainRef { ring_idx: 0, start: 0, len: 2, reversed: false },
            super::ChainRef { ring_idx: 1, start: 3, len: 2, reversed: true },
            super::ChainRef { ring_idx: 2, start: 1, len: 2, reversed: false },
        ],
    };
    let mut rings = vec![
        vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)],
        vec![(10, 0), (20, 0), (20, 10), (10, 10), (10, 0)],
        vec![(0, 0), (10, 0), (10, -10), (0, -10), (0, 0)],
    ];
    let result = super::canonicalize_shared_chains(&mut rings, &[chain]);
    assert_eq!(result.reconciled, 0);
    assert_eq!(result.skipped, 1);
}

// ---------------------------------------------------------------------------
// Tile-coordinate simplification tests
// ---------------------------------------------------------------------------

/// Simplify a ring with no pinned vertices — standard DP behavior.
#[test]
fn simplify_ring_tile_coords_no_pins() {
    // A square with a collinear midpoint on one edge.
    let ring = vec![(0, 0), (500, 0), (1000, 0), (1000, 1000), (0, 1000), (0, 0)];
    let pinned = vec![false; ring.len()];
    let simplified = super::simplify_ring_tile_coords(&ring, &pinned, 16.0);
    // (500, 0) is collinear with (0,0)→(1000,0), should be removed.
    assert_eq!(simplified.len(), 5, "collinear point should be removed");
    assert!(!simplified.contains(&(500, 0)));
}

/// Simplify a ring where shared-chain vertices are pinned — they survive.
#[test]
fn simplify_ring_tile_coords_with_pins() {
    // Same ring but (500, 0) is pinned (part of a shared chain).
    let ring = vec![(0, 0), (500, 0), (1000, 0), (1000, 1000), (0, 1000), (0, 0)];
    let pinned = vec![true, true, true, false, false, true]; // first 3 are shared chain
    let simplified = super::simplify_ring_tile_coords(&ring, &pinned, 16.0);
    // (500, 0) must survive because it's pinned.
    assert!(simplified.contains(&(500, 0)), "pinned vertex must survive");
}

/// Build pinned mask marks correct vertices.
#[test]
fn build_pinned_mask_basic() {
    let chain = super::SharedChain {
        vertices: vec![(10, 0), (10, 10)],
        incidents: vec![
            super::ChainRef { ring_idx: 0, start: 1, len: 2, reversed: false },
            super::ChainRef { ring_idx: 1, start: 3, len: 2, reversed: true },
        ],
    };
    // Ring 0 has 5 vertices (4 + close).
    let mask = super::build_pinned_mask(5, 0, std::slice::from_ref(&chain));
    assert_eq!(mask, vec![false, true, true, false, false]);

    // Ring 1: start=3, len=2 → positions 3, 0.
    let mask = super::build_pinned_mask(5, 1, std::slice::from_ref(&chain));
    // Position 3 and position 0 are pinned. Position 0 pinned → closing vertex (4) also pinned.
    assert_eq!(mask, vec![true, false, false, true, true]);
}
