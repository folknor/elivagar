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
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
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
pub fn for_each_clipped_segment<F>(line: &[Point], rect: &ClipRect, mut callback: F)
where
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
                    if current.len() >= 2 {
                        callback(current);
                    }
                    current.clear();
                    current.push(a);
                } else if current.is_empty() {
                    current.push(a);
                }
                current.push(b);

                if exits_to_outside {
                    if current.len() >= 2 {
                        callback(current);
                    }
                    current.clear();
                }
            } else {
                if current.len() >= 2 {
                    callback(current);
                }
                current.clear();
            }
        }

        if current.len() >= 2 {
            callback(current);
        }
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
