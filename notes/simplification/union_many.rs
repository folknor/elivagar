//! Hierarchical polygon union for tile-local ocean dissolve.
//!
//! Uses balanced pairwise reduction (Tilemaker's `union_many` strategy) instead
//! of sequential accumulation. Each round merges adjacent pairs, halving the
//! polygon count. This keeps intermediate results roughly input-sized, bounding
//! memory usage - unlike sequential union where the accumulator grows monotonically.
//!
//! Algorithm from tilemaker/src/geom.cpp:150-169, origin:
//! <https://github.com/boostorg/geometry/discussions/947>

use geo::BooleanOps;
use geo_types::{Coord, LineString, MultiPolygon, Polygon};

/// Polygon as a list of rings (first = exterior CW, rest = holes CCW).
/// Coordinates are i32 tile coords (0-4096 extent).
pub(crate) type TilePoly = Vec<Vec<(i32, i32)>>;

/// Hole-preserving ocean dissolve: union outers, union holes, subtract.
///
/// Plain union erases island holes when a blanket polygon (no hole) overlaps
/// a polygon with a hole - geometrically correct but semantically wrong for
/// ocean tiles. This function:
/// 1. Extracts all CW outer rings and all CCW hole rings from inputs
/// 2. Unions outers into W (water coverage)
/// 3. Unions holes into L (land/islands), treating each as a simple polygon
/// 4. Returns W - L (difference), preserving any hole from any input polygon
pub(crate) fn hole_preserving_dissolve(polygons: Vec<TilePoly>) -> Vec<TilePoly> {
    if polygons.len() < 2 {
        return polygons;
    }

    // Separate outers (CW, positive area) and holes (CCW, negative area).
    let mut outers: Vec<TilePoly> = Vec::new();
    let mut holes: Vec<TilePoly> = Vec::new();

    for poly in &polygons {
        if poly.is_empty() {
            continue;
        }
        // First ring is exterior
        outers.push(vec![poly[0].clone()]);
        // Remaining rings are holes - convert each to a simple outer polygon
        // (reverse winding so union_many treats them as polygons, not holes)
        for hole in poly.iter().skip(1) {
            let mut reversed = hole.clone();
            reversed.reverse();
            holes.push(vec![reversed]);
        }
    }

    // Union all outer rings into combined water coverage
    let water = union_many(outers);

    // If no holes, just return the unioned water
    if holes.is_empty() {
        return water;
    }

    // Union all holes into combined land coverage
    let land = union_many(holes);

    // Convert to geo types for difference operation
    let water_mp = tile_polys_to_multi(&water);
    let land_mp = tile_polys_to_multi(&land);

    let result = water_mp.difference(&land_mp);
    let out = geo_to_tile_polys(&result);
    normalize_tile_polys(out)
}

/// Convert Vec<TilePoly> to a single MultiPolygon for boolean ops.
fn tile_polys_to_multi(polys: &[TilePoly]) -> MultiPolygon<f64> {
    let mut geos = Vec::with_capacity(polys.len());
    for p in polys {
        let mp = tile_poly_to_geo(p);
        geos.extend(mp.0);
    }
    MultiPolygon(geos)
}

/// Union all input polygons into a minimal set of non-overlapping polygons.
///
/// Returns the dissolved result, or the original input unchanged if there are
/// fewer than 2 polygons. The `geo` crate's `BooleanOps::union` does the
/// actual geometry work; this function provides the hierarchical reduction
/// strategy that makes it tractable for complex inputs.
pub(crate) fn union_many(polygons: Vec<TilePoly>) -> Vec<TilePoly> {
    if polygons.len() < 2 {
        return polygons;
    }

    let mut to_unify: Vec<MultiPolygon<f64>> = polygons
        .into_iter()
        .map(|p| tile_poly_to_geo(&p))
        .collect();

    // Balanced pairwise reduction: merge (0,1), (2,3), … then (0,2), (4,6), …
    // until one result remains at index 0.
    let mut step = 1_usize;
    while step < to_unify.len() {
        let half_step = step;
        step *= 2;
        let mut i = 0;
        while i + half_step < to_unify.len() {
            let empty = || MultiPolygon(vec![]);
            let right = std::mem::replace(&mut to_unify[i + half_step], empty());
            let left = std::mem::replace(&mut to_unify[i], empty());
            to_unify[i] = left.union(&right);
            i += step;
        }
    }

    let result = geo_to_tile_polys(&to_unify[0]);

    // Post-union normalization: deduplicate consecutive points created by
    // f64→i32 rounding, drop degenerate rings (< 4 points after dedup),
    // and ensure correct winding (CW exterior, CCW holes).
    normalize_tile_polys(result)
}

/// Post-union normalization to fix f64→i32 round-trip artifacts.
fn normalize_tile_polys(polys: Vec<TilePoly>) -> Vec<TilePoly> {
    let mut out = Vec::with_capacity(polys.len());
    for poly in polys {
        let mut rings = Vec::with_capacity(poly.len());
        for (ring_idx, ring) in poly.into_iter().enumerate() {
            let clean = dedup_consecutive(&ring);
            if clean.len() < 4 {
                continue; // degenerate after rounding
            }
            let area = crate::geometry::signed_area_tile(&clean);
            if area.abs() < 0.5 {
                continue; // sliver polygon
            }
            let mut normalized = clean;
            if ring_idx == 0 {
                // Exterior must be CW (positive area in y-down tile coords)
                if area < 0.0 {
                    normalized.reverse();
                }
            } else {
                // Hole must be CCW (negative area)
                if area > 0.0 {
                    normalized.reverse();
                }
            }
            rings.push(normalized);
        }
        if !rings.is_empty() {
            out.push(rings);
        }
    }
    out
}

/// Remove consecutive duplicate points (from f64 rounding to same i32).
fn dedup_consecutive(ring: &[(i32, i32)]) -> Vec<(i32, i32)> {
    if ring.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(ring.len());
    out.push(ring[0]);
    for &pt in &ring[1..] {
        if pt != *out.last().expect("just pushed") {
            out.push(pt);
        }
    }
    // If first == last (closed ring), keep closing point but ensure
    // no other consecutive dups were created by the closure
    out
}

fn tile_poly_to_geo(rings: &[Vec<(i32, i32)>]) -> MultiPolygon<f64> {
    if rings.is_empty() {
        return MultiPolygon(vec![]);
    }
    let exterior = ring_to_linestring(&rings[0]);
    let holes: Vec<LineString<f64>> = rings.iter().skip(1).map(|r| ring_to_linestring(r)).collect();
    MultiPolygon(vec![Polygon::new(exterior, holes)])
}

fn ring_to_linestring(ring: &[(i32, i32)]) -> LineString<f64> {
    LineString(
        ring.iter()
            .map(|&(x, y)| Coord {
                x: f64::from(x),
                y: f64::from(y),
            })
            .collect(),
    )
}

#[allow(clippy::cast_possible_truncation)]
fn geo_to_tile_polys(mp: &MultiPolygon<f64>) -> Vec<TilePoly> {
    mp.0.iter()
        .map(|poly| {
            let mut rings = Vec::with_capacity(1 + poly.interiors().len());
            rings.push(linestring_to_ring(poly.exterior()));
            for hole in poly.interiors() {
                rings.push(linestring_to_ring(hole));
            }
            rings
        })
        .collect()
}

#[allow(clippy::cast_possible_truncation)]
fn linestring_to_ring(ls: &LineString<f64>) -> Vec<(i32, i32)> {
    ls.0.iter()
        .map(|c| (c.x.round() as i32, c.y.round() as i32))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn rect(x1: i32, y1: i32, x2: i32, y2: i32) -> TilePoly {
        vec![vec![
            (x1, y1), (x2, y1), (x2, y2), (x1, y2), (x1, y1),
        ]]
    }

    #[test]
    fn empty_input() {
        assert!(union_many(vec![]).is_empty());
    }

    #[test]
    fn single_polygon() {
        let result = union_many(vec![rect(0, 0, 100, 100)]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn two_overlapping_rectangles() {
        let result = union_many(vec![rect(0, 0, 200, 100), rect(100, 0, 300, 100)]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn three_overlapping_rectangles() {
        let result = union_many(vec![
            rect(0, 0, 200, 100),
            rect(100, 0, 300, 100),
            rect(200, 0, 400, 100),
        ]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn non_overlapping_polygons() {
        let result = union_many(vec![rect(0, 0, 100, 100), rect(500, 500, 600, 600)]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn polygon_with_hole_plus_overlapping() {
        // Outer with hole, plus filler fully inside the hole.
        // geo produces 2 polygons: outer-with-hole + filler island - geometrically correct.
        let exterior = vec![(0, 0), (400, 0), (400, 400), (0, 400), (0, 0)];
        let hole = vec![(100, 100), (100, 300), (300, 300), (300, 100), (100, 100)];
        let poly_with_hole: TilePoly = vec![exterior, hole];
        let filler = rect(150, 150, 250, 250);
        let result = union_many(vec![poly_with_hole, filler]);
        assert_eq!(result.len(), 2, "outer-with-hole + filler island: {result:?}");
    }

    #[test]
    fn polygon_with_hole_partially_filled() {
        // Filler overlaps the hole edge - should reduce hole size, single polygon result.
        let exterior = vec![(0, 0), (400, 0), (400, 400), (0, 400), (0, 0)];
        let hole = vec![(100, 100), (100, 300), (300, 300), (300, 100), (100, 100)];
        let poly_with_hole: TilePoly = vec![exterior, hole];
        // Filler overlaps right edge of outer, filling right side of hole
        let filler = rect(250, 0, 500, 400);
        let result = union_many(vec![poly_with_hole, filler]);
        assert_eq!(result.len(), 1, "should merge into single polygon: {result:?}");
    }

    #[test]
    fn stress_20_overlapping() {
        let input: Vec<TilePoly> = (0..20).map(|i| rect(i * 50, 0, i * 50 + 100, 100)).collect();
        assert_eq!(union_many(input).len(), 1);
    }

    #[test]
    fn adjacent_sharing_edge() {
        let result = union_many(vec![rect(0, 0, 100, 100), rect(100, 0, 200, 100)]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn odd_count() {
        let input: Vec<TilePoly> = (0..5).map(|i| rect(i * 50, 0, i * 50 + 100, 100)).collect();
        assert_eq!(union_many(input).len(), 1);
    }

    #[test]
    fn power_of_two() {
        let input: Vec<TilePoly> = (0..8).map(|i| rect(i * 50, 0, i * 50 + 100, 100)).collect();
        assert_eq!(union_many(input).len(), 1);
    }

    #[test]
    fn identical_polygons() {
        let result = union_many(vec![rect(0, 0, 100, 100), rect(0, 0, 100, 100)]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn normalization_drops_slivers() {
        // Two rectangles that after union might produce tiny sliver artifacts
        let result = union_many(vec![rect(0, 0, 100, 100), rect(100, 0, 200, 100)]);
        // All output rings should have >= 4 points
        for poly in &result {
            for ring in poly {
                assert!(ring.len() >= 4, "degenerate ring: {ring:?}");
            }
        }
    }
}
