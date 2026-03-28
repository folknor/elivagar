// Geometry engine: projection, simplification, clipping for vector tile generation.
//
// All operations work in Mercator [0,1] coordinate space unless stated otherwise.
// Pure Rust aside from smallvec for inline small-vec returns.

pub mod projection;
pub mod simplify;
pub mod clip;
mod surface;
pub mod tiles;
pub mod seams;
pub mod mvt_decode;

// Re-export everything at the geometry:: level so callers don't change.
pub use projection::*;
pub use simplify::*;
pub use clip::*;
pub use surface::*;
pub use tiles::*;
pub use seams::*;
pub use mvt_decode::*;

use std::f64::consts::PI;

/// MVT tile extent (pixels per tile axis).
pub const EXTENT: f64 = 4096.0;

/// Simplification tolerance in rendered pixels (one tile = 256×256 px).
/// 1.0 = vertices within 1 pixel of the simplified line are removed.
/// Industry standard is 1.0; Tilemaker uses ~0.58 for comparison.
const SIMPLIFY_PIXELS: f64 = 1.0;

/// Buffer fraction of tile size for clipping (8 rendered pixels).
/// One tile = 256×256 rendered pixels; 8/256 of the tile width.
pub(crate) const BUFFER_FRACTION: f64 = 8.0 / 256.0;

/// One tile pixel in extent units: 4096 / 256 = 16.
#[allow(clippy::cast_possible_truncation)]
const PX: i64 = (EXTENT as i64) / 256;

/// Minimum line length squared in tile extent units (1 pixel = 16 extent units).
/// A line whose bounding box diagonal² is below this is sub-pixel and invisible.
pub const MIN_LINE_EXTENT_SQ: i64 = PX * PX; // 256

/// Minimum polygon area in tile extent units² (1 sq pixel).
/// Polygon rings with |area| below this contribute nothing visible.
pub const MIN_POLY_AREA: i64 = PX * PX; // 256

/// Check if a geometry's Mercator bounding box diagonal is sub-pixel at the
/// given zoom level. One pixel = 1 / (256 × 2^z) Mercator units. Used as a
/// pre-simplification early exit: if the entire geometry is sub-pixel, DP is
/// pointless and all coarser zooms can be skipped too.
pub(crate) fn merc_bbox_is_subpixel(points: &[Point], zoom: u8) -> bool {
    if points.len() < 2 {
        return true;
    }
    let (mut min_x, mut min_y) = (points[0].x, points[0].y);
    let (mut max_x, mut max_y) = (min_x, min_y);
    for p in &points[1..] {
        if p.x < min_x { min_x = p.x; }
        if p.x > max_x { max_x = p.x; }
        if p.y < min_y { min_y = p.y; }
        if p.y > max_y { max_y = p.y; }
    }
    let pixel = 1.0 / (256.0 * f64::from(1u32 << zoom));
    let dx = max_x - min_x;
    let dy = max_y - min_y;
    dx * dx + dy * dy < pixel * pixel
}

/// Check if a linestring's bounding box is sub-pixel (both dimensions < 1 px).
/// Returns true if the feature is too small and should be dropped.
pub fn line_is_subpixel(coords: &[(i32, i32)]) -> bool {
    if coords.len() < 2 {
        return true;
    }
    let (mut min_x, mut min_y) = coords[0];
    let (mut max_x, mut max_y) = (min_x, min_y);
    for &(x, y) in &coords[1..] {
        if x < min_x { min_x = x; }
        if x > max_x { max_x = x; }
        if y < min_y { min_y = y; }
        if y > max_y { max_y = y; }
    }
    let dx = i64::from(max_x - min_x);
    let dy = i64::from(max_y - min_y);
    dx * dx + dy * dy < MIN_LINE_EXTENT_SQ
}

/// Check if a polygon ring's area is sub-pixel. Uses the shoelace formula.
/// Returns true if the feature is too small and should be dropped.
pub fn ring_is_subpixel(coords: &[(i32, i32)]) -> bool {
    if coords.len() < 4 {
        return true;
    }
    let mut area: i64 = 0;
    let n = coords.len();
    for i in 0..n {
        let j = (i + 1) % n;
        area += i64::from(coords[i].0) * i64::from(coords[j].1);
        area -= i64::from(coords[j].0) * i64::from(coords[i].1);
    }
    area.abs() / 2 < MIN_POLY_AREA
}

// ---------------------------------------------------------------------------
// Ring orientation (Shoelace formula)
// ---------------------------------------------------------------------------

/// Compute the signed area of a ring using the Shoelace formula.
///
/// Positive = counter-clockwise, negative = clockwise.
/// The ring should NOT have a duplicated closing vertex (we wrap automatically).
pub fn signed_area(ring: &[Point]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    let n = ring.len();
    for i in 0..n {
        let j = (i + 1) % n;
        sum += ring[i].x * ring[j].y;
        sum -= ring[j].x * ring[i].y;
    }
    sum * 0.5
}

/// Returns `true` if the ring is wound counter-clockwise.
#[cfg(test)]
#[inline]
pub fn is_ccw(ring: &[Point]) -> bool {
    signed_area(ring) > 0.0
}

/// Returns `true` if the ring is wound clockwise.
#[cfg(test)]
#[inline]
pub fn is_cw(ring: &[Point]) -> bool {
    signed_area(ring) < 0.0
}

/// Reverse a ring's winding order in place.
#[cfg(test)]
pub fn reverse_ring(ring: &mut [Point]) {
    ring.reverse();
}

// ---------------------------------------------------------------------------
// Area calculation in square meters (approximate)
// ---------------------------------------------------------------------------

/// Area of a ring in square meters using per-edge latitude correction.
///
/// The ring is in Mercator [0,1] space. Mercator is conformal, so local scale
/// at latitude φ is C·cos(φ) in both x and y. The real-world area element is
/// `C² · cos²(φ(y)) · dx dy`. Via Green's theorem this becomes the line
/// integral `∮ x · cos²(φ(y)) dy`, discretized per edge with midpoint rule.
///
/// cos²(φ) is computed directly from Mercator y: cos²(lat(y)) = sech²(π(1-2y)).
/// One cosh() call per edge, O(n). Not on the hotpath.
pub fn area_sq_meters(ring: &[Point]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let n = ring.len();
    let c_sq = projection::EARTH_CIRCUMFERENCE * projection::EARTH_CIRCUMFERENCE;
    let mut sum = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        let dy = ring[j].y - ring[i].y;
        let x_sum = ring[i].x + ring[j].x;
        let y_mid = (ring[i].y + ring[j].y) * 0.5;
        // cos²(lat(y)) = sech²(π(1 - 2y)) = 1/cosh²(π(1 - 2y))
        let arg = PI * (1.0 - 2.0 * y_mid);
        let cosh_val = arg.cosh();
        let cos_sq_lat = 1.0 / (cosh_val * cosh_val);
        sum += x_sum * dy * cos_sq_lat;
    }
    (sum.abs() * 0.5) * c_sq
}

// ---------------------------------------------------------------------------
// Point-in-polygon (ray casting)
// ---------------------------------------------------------------------------

/// Ray-casting point-in-polygon test. Returns true if `p` is inside `ring`.
///
/// Consolidated from ocean.rs and multipolygon.rs which both had independent
/// implementations of the same ray-casting algorithm.
pub fn point_in_polygon(p: &Point, ring: &[Point]) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let yi = ring[i].y;
        let yj = ring[j].y;
        if (yi > p.y) != (yj > p.y) {
            let intersect_x = ring[i].x + (p.y - yi) / (yj - yi) * (ring[j].x - ring[i].x);
            if p.x < intersect_x {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

// ---------------------------------------------------------------------------
// Interior tile detection
// ---------------------------------------------------------------------------

/// Check if a tile (given by its buffered `ClipRect`) is entirely interior to a polygon ring.
///
/// Single O(n) pass combining 4-corner PIP ray-cast with edge-bbox overlap check.
/// Returns true when no edge bbox overlaps the clip rect AND all 4 corners of the
/// clip rect are inside the ring. Conservative: may return false for tiles that are
/// truly interior but have a nearby edge bbox.
pub fn tile_is_interior(ring: &[Point], clip: &ClipRect) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }

    let corners: [(f64, f64); 4] = [
        (clip.min_x, clip.min_y),
        (clip.max_x, clip.min_y),
        (clip.max_x, clip.max_y),
        (clip.min_x, clip.max_y),
    ];
    let mut inside = [false; 4];

    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (ring[i].x, ring[i].y);
        let (xj, yj) = (ring[j].x, ring[j].y);

        // Edge bbox overlap with clip rect — if any edge might cross the tile,
        // the polygon boundary could intersect it, so bail out conservatively.
        if xi.min(xj) <= clip.max_x
            && xi.max(xj) >= clip.min_x
            && yi.min(yj) <= clip.max_y
            && yi.max(yj) >= clip.min_y
        {
            return false;
        }

        // PIP ray-casting for all 4 corners simultaneously.
        for (k, &(cx, cy)) in corners.iter().enumerate() {
            if (yi > cy) != (yj > cy) {
                let intersect_x = xi + (cy - yi) / (yj - yi) * (xj - xi);
                if cx < intersect_x {
                    inside[k] = !inside[k];
                }
            }
        }

        j = i;
    }

    inside[0] && inside[1] && inside[2] && inside[3]
}

// ---------------------------------------------------------------------------
// Debug ring validation (ocean geometry diagnostics)
// ---------------------------------------------------------------------------

/// Check a set of tile-coordinate rings for geometry defects.
/// Returns a list of human-readable problem descriptions.
/// Used to isolate where ocean polygon corruption originates.
pub fn debug_check_rings(rings: &[Vec<(i32, i32)>]) -> Vec<String> {
    let mut problems = Vec::new();
    for (ri, ring) in rings.iter().enumerate() {
        if ring.len() < 4 {
            problems.push(format!("ring {ri}: degenerate ({} verts)", ring.len()));
            continue;
        }

        // Consecutive duplicate vertices
        for i in 0..ring.len() - 1 {
            if ring[i] == ring[i + 1] {
                problems.push(format!(
                    "ring {ri}: consecutive dup at {i}: ({}, {})",
                    ring[i].0, ring[i].1
                ));
            }
        }

        // Immediate A→B→A backtracks
        if ring.len() >= 3 {
            for i in 0..ring.len() - 2 {
                if ring[i] == ring[i + 2] && ring[i] != ring[i + 1] {
                    problems.push(format!(
                        "ring {ri}: backtrack at {i}: ({},{})→({},{})→({},{})",
                        ring[i].0, ring[i].1,
                        ring[i + 1].0, ring[i + 1].1,
                        ring[i + 2].0, ring[i + 2].1,
                    ));
                }
            }
        }

        // Simple self-intersection: check if any non-adjacent edges cross.
        // Only test a sample to avoid O(n²) on large rings.
        let n = ring.len() - 1; // exclude closing vertex
        if n >= 4 {
            let step = if n > 200 { n / 100 } else { 1 };
            for i in (0..n).step_by(step) {
                let a1 = ring[i];
                let a2 = ring[(i + 1) % n];
                // Check against non-adjacent edges
                let j_start = (i + 2) % n;
                for jj in 0..n.min(20) {
                    let j = (j_start + jj) % n;
                    if j == i || (j + 1) % n == i {
                        continue;
                    }
                    let b1 = ring[j];
                    let b2 = ring[(j + 1) % n];
                    if segments_cross(a1, a2, b1, b2) {
                        problems.push(format!(
                            "ring {ri}: self-intersection edges {i}-{} and {j}-{}",
                            (i + 1) % n, (j + 1) % n
                        ));
                        break; // one per ring is enough
                    }
                }
                if problems.iter().any(|p| p.contains(&format!("ring {ri}: self"))) {
                    break;
                }
            }
        }
    }
    problems
}

/// Check if a closed ring (first == last) has any self-intersections.
/// Returns true if the ring is simple (no crossings). O(n²) but rings are
/// typically small after DP simplification.
pub fn ring_is_simple(ring: &[(i32, i32)]) -> bool {
    if ring.len() < 4 {
        return true;
    }
    let n = ring.len() - 1; // exclude closing vertex
    for i in 0..n {
        let a1 = ring[i];
        let a2 = ring[i + 1];
        // Check against non-adjacent edges
        for j in (i + 2)..n {
            if j + 1 == ring.len() && i == 0 {
                continue; // last edge wraps to first — they share a vertex
            }
            let b1 = ring[j];
            let b2 = ring[(j + 1) % ring.len()];
            if segments_cross(a1, a2, b1, b2) {
                return false;
            }
        }
    }
    true
}

/// Test if two line segments (a1-a2) and (b1-b2) properly cross (not just touch).
fn segments_cross(a1: (i32, i32), a2: (i32, i32), b1: (i32, i32), b2: (i32, i32)) -> bool {
    let d1 = cross_sign(a1, a2, b1);
    let d2 = cross_sign(a1, a2, b2);
    let d3 = cross_sign(b1, b2, a1);
    let d4 = cross_sign(b1, b2, a2);
    // Proper crossing: endpoints of each segment on opposite sides of the other
    d1 != d2 && d3 != d4 && d1 != 0 && d2 != 0 && d3 != 0 && d4 != 0
}

/// Sign of the cross product (p2-p1) × (p3-p1).
fn cross_sign(p1: (i32, i32), p2: (i32, i32), p3: (i32, i32)) -> i8 {
    let cross = i64::from(p2.0 - p1.0) * i64::from(p3.1 - p1.1)
              - i64::from(p2.1 - p1.1) * i64::from(p3.0 - p1.0);
    if cross > 0 { 1 } else if cross < 0 { -1 } else { 0 }
}

/// Compute the intersection point of two properly-crossing line segments.
/// Uses parametric form with f64 for precision, rounds to integer coords.
#[allow(clippy::cast_possible_truncation)]
fn segment_intersection(a1: (i32, i32), a2: (i32, i32), b1: (i32, i32), b2: (i32, i32)) -> (i32, i32) {
    let d1x = f64::from(a2.0 - a1.0);
    let d1y = f64::from(a2.1 - a1.1);
    let d2x = f64::from(b2.0 - b1.0);
    let d2y = f64::from(b2.1 - b1.1);
    let denom = d1x * d2y - d1y * d2x;
    if denom.abs() < 1e-12 {
        return a1; // parallel — shouldn't happen for properly-crossing segments
    }
    let dx = f64::from(b1.0 - a1.0);
    let dy = f64::from(b1.1 - a1.1);
    let t = (dx * d2y - dy * d2x) / denom;
    let x = f64::from(a1.0) + t * d1x;
    let y = f64::from(a1.1) + t * d1y;
    (x.round() as i32, y.round() as i32)
}

/// Split a self-intersecting (figure-8) closed ring at crossing points.
/// Returns None if the ring is already simple.
/// Returns Some(vec) of simple sub-rings (each closed, first == last, ≥4 verts).
/// Recurses to handle rings with multiple crossings.
pub(crate) fn split_figure8_ring(ring: &[(i32, i32)]) -> Option<Vec<Vec<(i32, i32)>>> {
    if ring.len() < 5 { return None; } // need ≥4 unique + closing
    let n = ring.len() - 1; // number of unique vertices (ring[n] == ring[0])

    for i in 0..n {
        let a1 = ring[i];
        let a2 = ring[i + 1];
        for j in (i + 2)..n {
            // Skip adjacent edges (last edge wraps to first — they share a vertex)
            if j + 1 == ring.len() && i == 0 { continue; }
            let b1 = ring[j];
            let b2 = ring[(j + 1) % ring.len()];
            if !segments_cross(a1, a2, b1, b2) { continue; }

            let p = segment_intersection(a1, a2, b1, b2);

            // Sub-ring A: p → ring[i+1..=j] → p
            let mut ra = Vec::with_capacity(j - i + 2);
            ra.push(p);
            for k in (i + 1)..=j { ra.push(ring[k]); }
            ra.push(p);

            // Sub-ring B: p → ring[j+1..n-1] → ring[0..=i] → p
            let mut rb = Vec::with_capacity(n - (j - i) + 2);
            rb.push(p);
            for k in (j + 1)..n { rb.push(ring[k]); }
            for k in 0..=i { rb.push(ring[k]); }
            rb.push(p);

            // Recursively split sub-rings if they still self-intersect
            let mut result = Vec::new();
            for sub in [ra, rb] {
                if sub.len() < 4 { continue; }
                if let Some(splits) = split_figure8_ring(&sub) {
                    result.extend(splits);
                } else {
                    result.push(sub);
                }
            }
            return if result.is_empty() { None } else { Some(result) };
        }
    }
    None
}

/// Nudge hole vertices that share exact coordinates with any outer ring vertex.
/// Displaces by 1 extent unit toward hole centroid. Prevents earcut bridge
/// degeneration when hole and outer ring vertices coincide at tile boundaries.
pub(crate) fn nudge_coincident_hole_vertices(hole: &mut [(i32, i32)], outer: &[(i32, i32)]) {
    if hole.len() < 4 { return; }
    let n = hole.len() - 1; // exclude closing vertex
    // Compute hole centroid
    let (mut cx, mut cy) = (0i64, 0i64);
    for &(x, y) in &hole[..n] {
        cx += i64::from(x);
        cy += i64::from(y);
    }
    cx /= n as i64;
    cy /= n as i64;
    // Nudge matching vertices 1 unit toward centroid
    let outer_unique = &outer[..outer.len().saturating_sub(1)];
    for i in 0..n {
        if outer_unique.iter().any(|o| *o == hole[i]) {
            let dx = if cx > i64::from(hole[i].0) { 1 } else { -1 };
            let dy = if cy > i64::from(hole[i].1) { 1 } else { -1 };
            hole[i].0 += dx;
            hole[i].1 += dy;
        }
    }
    // Fix closing vertex
    hole[n] = hole[0];
}

/// Nudge hole vertices that sit exactly on the clip rect boundary.
/// After S-H clipping, both outer and inner rings produce vertices at the
/// clip rect corners/edges. Earcut's bridge algorithm fails when hole and
/// outer vertices are coincident. Nudging by 1 extent unit inward (1/16 pixel)
/// breaks the coincidence without visible effect.
pub(crate) fn nudge_hole_off_boundary(ring: &mut [(i32, i32)]) {
    if ring.len() < 4 { return; }
    #[allow(clippy::cast_possible_truncation)]
    let buf: i32 = (BUFFER_FRACTION * EXTENT) as i32; // 128
    #[allow(clippy::cast_possible_truncation)]
    let ext: i32 = EXTENT as i32; // 4096
    let (min, max) = (-buf, ext + buf);
    let n = ring.len() - 1;
    for v in ring[..n].iter_mut() {
        if v.0 == min { v.0 += 1; }
        else if v.0 == max { v.0 -= 1; }
        if v.1 == min { v.1 += 1; }
        else if v.1 == max { v.1 -= 1; }
    }
    ring[n] = ring[0];
}

/// Check if a closed ring of Mercator Points has any self-intersections.
/// f64 version of `ring_is_simple` — used to detect S-H figure-8s BEFORE
/// quantization, where proper crossings are still detectable.
pub(crate) fn ring_is_simple_merc(ring: &[Point]) -> bool {
    if ring.len() < 4 {
        return true;
    }
    let n = ring.len() - 1;
    for i in 0..n {
        let a1 = ring[i];
        let a2 = ring[i + 1];
        for j in (i + 2)..n {
            if j + 1 == ring.len() && i == 0 {
                continue;
            }
            let b1 = ring[j];
            let b2 = ring[(j + 1) % ring.len()];
            if segments_cross_f64(a1, a2, b1, b2) {
                return false;
            }
        }
    }
    true
}

fn segments_cross_f64(a1: Point, a2: Point, b1: Point, b2: Point) -> bool {
    let d1 = cross_sign_f64(a1, a2, b1);
    let d2 = cross_sign_f64(a1, a2, b2);
    let d3 = cross_sign_f64(b1, b2, a1);
    let d4 = cross_sign_f64(b1, b2, a2);
    d1 != d2 && d3 != d4 && d1 != 0 && d2 != 0 && d3 != 0 && d4 != 0
}

fn cross_sign_f64(p1: Point, p2: Point, p3: Point) -> i8 {
    let cross = (p2.x - p1.x) * (p3.y - p1.y) - (p2.y - p1.y) * (p3.x - p1.x);
    if cross > 1e-15 { 1 } else if cross < -1e-15 { -1 } else { 0 }
}

// ---------------------------------------------------------------------------
// Tests (see geometry_tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../geometry_tests.rs"]
mod tests;
