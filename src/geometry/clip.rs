#[cfg(test)]
use smallvec::SmallVec;

use super::projection::Point;

/// Region outcodes for Cohen-Sutherland.
const INSIDE: u8 = 0b0000;
const LEFT: u8 = 0b0001;
const RIGHT: u8 = 0b0010;
const BOTTOM: u8 = 0b0100;
const TOP: u8 = 0b1000;

/// Compute the outcode for a point relative to a clipping rectangle.
#[inline]
fn outcode(p: &Point, rect: &ClipRect) -> u8 {
    let mut code = INSIDE;
    if p.x < rect.min_x {
        code |= LEFT;
    } else if p.x > rect.max_x {
        code |= RIGHT;
    }
    if p.y < rect.min_y {
        code |= BOTTOM;
    } else if p.y > rect.max_y {
        code |= TOP;
    }
    code
}

/// Axis-aligned clipping rectangle.
#[derive(Clone, Copy, Debug)]
pub struct ClipRect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl ClipRect {
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self { min_x, min_y, max_x, max_y }
    }

    /// Create a clip rect for a tile with optional buffer (in fraction of tile size).
    pub fn for_tile(tile_x: u32, tile_y: u32, zoom: u8, buffer_fraction: f64) -> Self {
        let z_scale = f64::from(1u32 << zoom);
        let inv_z = 1.0 / z_scale;
        let x0 = f64::from(tile_x) * inv_z;
        let y0 = f64::from(tile_y) * inv_z;
        let buf = buffer_fraction * inv_z;
        Self::new(x0 - buf, y0 - buf, x0 + inv_z + buf, y0 + inv_z + buf)
    }
}

/// O(1) AABB intersection test: does a Mercator bbox overlap a clip rect?
pub fn bbox_intersects_clip(bbox: &super::projection::MercBbox, clip: &ClipRect) -> bool {
    bbox.max_x >= clip.min_x
        && bbox.min_x <= clip.max_x
        && bbox.max_y >= clip.min_y
        && bbox.min_y <= clip.max_y
}

/// Clip a linestring to an axis-aligned rectangle using Cohen-Sutherland.
///
/// Returns zero or more sub-linestrings (the line may enter and exit multiple times).
/// SmallVec<[_; 1]>: most clips produce exactly one segment, avoiding the outer heap alloc.
#[cfg(test)]
pub fn clip_linestring(line: &[Point], rect: &ClipRect) -> SmallVec<[Vec<Point>; 1]> {
    let mut result: SmallVec<[Vec<Point>; 1]> = SmallVec::new();
    for_each_clipped_segment(line, rect, |segment| {
        result.push(segment.to_vec());
    });
    result
}

thread_local! {
    static CLIP_LINE_SCRATCH: std::cell::RefCell<Vec<Point>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Clip a linestring and call `callback` for each visible sub-segment.
///
/// Uses a thread-local scratch buffer to avoid per-call allocation. The callback
/// receives a borrowed slice of the segment points.
#[hotpath::measure]
pub fn for_each_clipped_segment<F>(
    line: &[Point],
    rect: &ClipRect,
    mut callback: F,
) where
    F: FnMut(&[Point]),
{
    if line.len() < 2 {
        return;
    }
    // Outcode pre-test: reject entire linestring if all vertices are outside the same edge.
    let mut and_code = 0xFFu8;
    for p in line {
        and_code &= outcode(p, rect);
        if and_code == INSIDE {
            break;
        }
    }
    if and_code != INSIDE {
        return;
    }
    CLIP_LINE_SCRATCH.with(|cell| {
    let current = &mut *cell.borrow_mut();
    current.clear();

    for i in 0..(line.len() - 1) {
        if let Some((a, b)) = clip_segment(line[i], line[i + 1], rect) {
            let enters_from_outside = !points_near(&a, &line[i]);
            let exits_to_outside = !points_near(&b, &line[i + 1]);

            if enters_from_outside {
                if current.len() >= 2 { callback(current); }
                current.clear();
                current.push(a);
            } else if current.is_empty() {
                current.push(a);
            }
            current.push(b);

            if exits_to_outside {
                if current.len() >= 2 { callback(current); }
                current.clear();
            }
        } else {
            if current.len() >= 2 { callback(current); }
            current.clear();
        }
    }

    if current.len() >= 2 { callback(current); }
    }); // CLIP_LINE_SCRATCH.with
}

/// Test whether two points are essentially the same (within floating-point tolerance).
#[inline]
fn points_near(a: &Point, b: &Point) -> bool {
    (a.x - b.x).abs() < 1e-12 && (a.y - b.y).abs() < 1e-12
}

/// Clip a single line segment to the rectangle. Returns `None` if fully outside.
fn clip_segment(mut p0: Point, mut p1: Point, rect: &ClipRect) -> Option<(Point, Point)> {
    let mut code0 = outcode(&p0, rect);
    let mut code1 = outcode(&p1, rect);

    loop {
        if (code0 | code1) == INSIDE {
            return Some((p0, p1));
        }
        if (code0 & code1) != INSIDE {
            return None;
        }

        let code_out = if code0 != INSIDE { code0 } else { code1 };
        let clipped = intersect_edge(&p0, &p1, rect, code_out);

        if code_out == code0 {
            p0 = clipped;
            code0 = outcode(&p0, rect);
        } else {
            p1 = clipped;
            code1 = outcode(&p1, rect);
        }
    }
}

/// Find the intersection of the line p0→p1 with the rectangle edge indicated by `code`.
fn intersect_edge(p0: &Point, p1: &Point, rect: &ClipRect, code: u8) -> Point {
    let dx = p1.x - p0.x;
    let dy = p1.y - p0.y;

    if (code & TOP) != 0 {
        let t = (rect.max_y - p0.y) / dy;
        Point::new(p0.x + t * dx, rect.max_y)
    } else if (code & BOTTOM) != 0 {
        let t = (rect.min_y - p0.y) / dy;
        Point::new(p0.x + t * dx, rect.min_y)
    } else if (code & RIGHT) != 0 {
        let t = (rect.max_x - p0.x) / dx;
        Point::new(rect.max_x, p0.y + t * dy)
    } else {
        // LEFT
        let t = (rect.min_x - p0.x) / dx;
        Point::new(rect.min_x, p0.y + t * dy)
    }
}

// ---------------------------------------------------------------------------
// Sutherland-Hodgman polygon clipping
// ---------------------------------------------------------------------------

/// Clip a polygon ring to an axis-aligned rectangle using Sutherland-Hodgman.
///
/// The input ring should NOT have a duplicated closing vertex.
/// Result is left in `buf_a` after the call (4 edge swaps = even = back to original).
/// Callers should hoist `buf_a`/`buf_b` outside inner loops to avoid per-call allocation.
#[hotpath::measure]
pub fn clip_polygon_into(
    ring: &[Point],
    rect: &ClipRect,
    buf_a: &mut Vec<Point>,
    buf_b: &mut Vec<Point>,
) {
    buf_a.clear();
    buf_b.clear();
    if ring.is_empty() {
        return;
    }
    // Outcode pre-test: if all vertices share a common outside bit (all left,
    // all right, etc.), the polygon is entirely outside one edge - skip S-H.
    // One O(n) pass with 4 comparisons per vertex vs S-H's 4×O(n) with
    // intersection math.
    let mut and_code = 0xFFu8;
    for p in ring {
        and_code &= outcode(p, rect);
        if and_code == INSIDE {
            break; // Can't reject early, must run S-H
        }
    }
    if and_code != INSIDE {
        return; // All vertices outside the same edge - guaranteed empty
    }
    buf_a.extend_from_slice(ring);
    for edge in [
        Edge::Left(rect.min_x),
        Edge::Right(rect.max_x),
        Edge::Bottom(rect.min_y),
        Edge::Top(rect.max_y),
    ] {
        clip_polygon_edge_into(buf_a, edge, buf_b);
        std::mem::swap(buf_a, buf_b);
        buf_b.clear();
    }
    // Result is in buf_a
}

/// Convenience wrapper that allocates its own buffers. Use [`clip_polygon_into`] in hot paths.
pub fn clip_polygon(ring: &[Point], rect: &ClipRect) -> Vec<Point> {
    let mut buf_a = Vec::new();
    let mut buf_b = Vec::new();
    clip_polygon_into(ring, rect, &mut buf_a, &mut buf_b);
    buf_a
}

/// Which rectangle edge we are clipping against, and its coordinate value.
#[derive(Clone, Copy)]
enum Edge {
    Left(f64),
    Right(f64),
    Bottom(f64),
    Top(f64),
}

/// Is the point on the "inside" side of this edge?
#[inline]
fn is_inside(p: &Point, edge: Edge) -> bool {
    match edge {
        Edge::Left(x) => p.x >= x,
        Edge::Right(x) => p.x <= x,
        Edge::Bottom(y) => p.y >= y,
        Edge::Top(y) => p.y <= y,
    }
}

/// Intersect segment s→e with the clipping edge.
///
/// Division by dx (Left/Right) or dy (Top/Bottom) is safe: the caller only
/// invokes this when one point is inside and the other outside the edge,
/// which guarantees the relevant denominator is nonzero.
#[inline]
fn edge_intersect(s: &Point, e: &Point, edge: Edge) -> Point {
    let dx = e.x - s.x;
    let dy = e.y - s.y;
    match edge {
        Edge::Left(x) | Edge::Right(x) => {
            let t = (x - s.x) / dx;
            Point::new(x, s.y + t * dy)
        }
        Edge::Bottom(y) | Edge::Top(y) => {
            let t = (y - s.y) / dy;
            Point::new(s.x + t * dx, y)
        }
    }
}

/// Clip a polygon against a single edge, appending results to `output`.
fn clip_polygon_edge_into(polygon: &[Point], edge: Edge, output: &mut Vec<Point>) {
    if polygon.is_empty() {
        return;
    }
    let mut s = polygon[polygon.len() - 1];
    for &e in polygon {
        let e_inside = is_inside(&e, edge);
        let s_inside = is_inside(&s, edge);
        if e_inside {
            if !s_inside {
                output.push(edge_intersect(&s, &e, edge));
            }
            output.push(e);
        } else if s_inside {
            output.push(edge_intersect(&s, &e, edge));
        }
        s = e;
    }
}

