// Geometry engine: projection, simplification, clipping for vector tile generation.
//
// All operations work in Mercator [0,1] coordinate space unless stated otherwise.
// Pure Rust — no external crates.

use std::f64::consts::PI;

/// Earth's equatorial circumference in meters.
const EARTH_CIRCUMFERENCE: f64 = 40_075_016.686;

/// Maximum latitude for Web Mercator (beyond this, projection diverges).
const MAX_LATITUDE: f64 = 85.051_129;

/// MVT tile extent (pixels per tile axis).
pub const EXTENT: f64 = 4096.0;

/// Sub-pixel simplification factor.
const PIXEL_FACTOR: f64 = 0.375;

/// Buffer fraction of tile size for clipping (8 pixels / 4096 extent).
pub(crate) const BUFFER_FRACTION: f64 = 8.0 / EXTENT;

// ---------------------------------------------------------------------------
// Point type
// ---------------------------------------------------------------------------

/// A 2D point in Mercator [0,1] space.
#[derive(Clone, Copy, Debug)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    #[inline]
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Axis-aligned bounding rectangle in Mercator [0,1] space.
#[derive(Clone, Copy, Debug)]
pub struct MercBbox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

// ---------------------------------------------------------------------------
// Projection: WGS84 → Mercator [0,1]
// ---------------------------------------------------------------------------

/// Project a single WGS84 coordinate (lat_deg, lon_deg) to Mercator [0,1].
#[inline]
pub fn project(lat_deg: f64, lon_deg: f64) -> Point {
    let lat_clamped = lat_deg.clamp(-MAX_LATITUDE, MAX_LATITUDE);
    let x = (lon_deg + 180.0) / 360.0;
    let lat_rad = lat_clamped * PI / 180.0;
    let y = 0.5 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / (2.0 * PI);
    Point::new(x, y)
}

/// Project from fixed-point e7 integers (as stored in the disk format) to Mercator [0,1].
#[inline]
pub fn project_e7(lat_e7: i32, lon_e7: i32) -> Point {
    let lat_deg = f64::from(lat_e7) * 1e-7;
    let lon_deg = f64::from(lon_e7) * 1e-7;
    project(lat_deg, lon_deg)
}

/// Convert EPSG:3857 (Web Mercator meters) to Mercator [0,1] coordinates.
#[inline]
pub fn from_epsg3857(x: f64, y: f64) -> Point {
    let half_c = EARTH_CIRCUMFERENCE / 2.0;
    Point::new(
        (x + half_c) / EARTH_CIRCUMFERENCE,
        (half_c - y) / EARTH_CIRCUMFERENCE,
    )
}

/// Inverse projection: Mercator y → latitude in degrees.
#[inline]
pub fn merc_y_to_lat(y: f64) -> f64 {
    let lat_rad = (PI * (1.0 - 2.0 * y)).sinh().atan();
    lat_rad * 180.0 / PI
}

// ---------------------------------------------------------------------------
// Tile coordinate conversion
// ---------------------------------------------------------------------------

/// Convert a Mercator [0,1] point to tile-local pixel coordinates.
///
/// Returns `(px_x, px_y)` as i32 suitable for MVT command encoding.
#[allow(clippy::cast_possible_truncation)]
pub fn merc_to_tile_px(p: &Point, tile_x: u32, tile_y: u32, zoom: u8) -> (i32, i32) {
    let z_scale = f64::from(1u32 << zoom);
    let px_x = (p.x * z_scale - f64::from(tile_x)) * EXTENT;
    let px_y = (p.y * z_scale - f64::from(tile_y)) * EXTENT;
    (px_x.round() as i32, px_y.round() as i32)
}

/// Compute the simplification tolerance for a given zoom level.
#[inline]
pub fn simplify_tolerance(zoom: u8) -> f64 {
    let z_scale = f64::from(1u32 << zoom);
    PIXEL_FACTOR / (EXTENT * z_scale)
}

// ---------------------------------------------------------------------------
// Douglas-Peucker simplification
// ---------------------------------------------------------------------------

/// Simplify a polyline using the Douglas-Peucker algorithm.
///
/// Points whose perpendicular distance to the line segment between endpoints
/// is less than `tolerance` are removed.
pub fn simplify(points: &[Point], tolerance: f64) -> Vec<Point> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    dp_recurse(points, 0, points.len() - 1, tolerance * tolerance, &mut keep);
    keep.iter()
        .enumerate()
        .filter(|&(_, &k)| k)
        .map(|(i, _)| points[i])
        .collect()
}

/// Recursive step of Douglas-Peucker. Uses squared tolerance to avoid sqrt.
fn dp_recurse(points: &[Point], start: usize, end: usize, tol_sq: f64, keep: &mut [bool]) {
    if end <= start + 1 {
        return;
    }
    let (max_idx, max_dist_sq) = find_farthest(points, start, end);
    if max_dist_sq > tol_sq {
        keep[max_idx] = true;
        dp_recurse(points, start, max_idx, tol_sq, keep);
        dp_recurse(points, max_idx, end, tol_sq, keep);
    }
}

/// Find the point farthest from the line segment `points[start]..points[end]`.
fn find_farthest(points: &[Point], start: usize, end: usize) -> (usize, f64) {
    let a = points[start];
    let b = points[end];
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len_sq = dx * dx + dy * dy;

    let mut max_dist_sq: f64 = 0.0;
    let mut max_idx = start;

    for i in (start + 1)..end {
        let dist_sq = perp_dist_sq(&points[i], &a, dx, dy, len_sq);
        if dist_sq > max_dist_sq {
            max_dist_sq = dist_sq;
            max_idx = i;
        }
    }
    (max_idx, max_dist_sq)
}

/// Squared perpendicular distance from `p` to the line through `a` with direction `(dx, dy)`.
#[inline]
fn perp_dist_sq(p: &Point, a: &Point, dx: f64, dy: f64, len_sq: f64) -> f64 {
    if len_sq < 1e-30 {
        // Degenerate segment: distance to point a
        let ex = p.x - a.x;
        let ey = p.y - a.y;
        return ex * ex + ey * ey;
    }
    let t = ((p.x - a.x) * dx + (p.y - a.y) * dy) / len_sq;
    let t_clamped = t.clamp(0.0, 1.0);
    let proj_x = a.x + t_clamped * dx;
    let proj_y = a.y + t_clamped * dy;
    let ex = p.x - proj_x;
    let ey = p.y - proj_y;
    ex * ex + ey * ey
}

// ---------------------------------------------------------------------------
// Cohen-Sutherland line clipping
// ---------------------------------------------------------------------------

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

/// Clip a linestring to an axis-aligned rectangle using Cohen-Sutherland.
///
/// Returns zero or more sub-linestrings (the line may enter and exit multiple times).
pub fn clip_linestring(line: &[Point], rect: &ClipRect) -> Vec<Vec<Point>> {
    if line.len() < 2 {
        return Vec::new();
    }
    let mut result: Vec<Vec<Point>> = Vec::new();
    let mut current: Vec<Point> = Vec::new();

    for i in 0..(line.len() - 1) {
        clip_segment_and_collect(
            line[i], line[i + 1], rect, &mut current, &mut result,
        );
    }

    if current.len() >= 2 {
        result.push(current);
    }
    result
}

/// Clip one segment and append visible portions to `current` / `result`.
fn clip_segment_and_collect(
    p0: Point,
    p1: Point,
    rect: &ClipRect,
    current: &mut Vec<Point>,
    result: &mut Vec<Vec<Point>>,
) {
    if let Some((a, b)) = clip_segment(p0, p1, rect) {
        let enters_from_outside = !points_near(&a, &p0);
        let exits_to_outside = !points_near(&b, &p1);

        if enters_from_outside {
            // Start a new sub-line at the entry point
            flush_segment(current, result);
            current.push(a);
        } else if current.is_empty() {
            current.push(a);
        }
        current.push(b);

        if exits_to_outside {
            flush_segment(current, result);
        }
    } else {
        // Segment entirely outside — flush any in-progress sub-line
        flush_segment(current, result);
    }
}

/// Move the contents of `current` into `result` if it has at least 2 points.
fn flush_segment(current: &mut Vec<Point>, result: &mut Vec<Vec<Point>>) {
    if current.len() >= 2 {
        result.push(std::mem::take(current));
    } else {
        current.clear();
    }
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
/// Returns the clipped ring (may be empty if fully outside).
pub fn clip_polygon(ring: &[Point], rect: &ClipRect) -> Vec<Point> {
    if ring.is_empty() {
        return Vec::new();
    }

    // Clip against each of the four edges sequentially.
    let after_left = clip_polygon_edge(ring, Edge::Left(rect.min_x));
    let after_right = clip_polygon_edge(&after_left, Edge::Right(rect.max_x));
    let after_bottom = clip_polygon_edge(&after_right, Edge::Bottom(rect.min_y));
    clip_polygon_edge(&after_bottom, Edge::Top(rect.max_y))
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

/// Clip a polygon against a single edge.
fn clip_polygon_edge(polygon: &[Point], edge: Edge) -> Vec<Point> {
    if polygon.is_empty() {
        return Vec::new();
    }
    let mut output = Vec::with_capacity(polygon.len());
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
    output
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
#[inline]
pub fn is_ccw(ring: &[Point]) -> bool {
    signed_area(ring) > 0.0
}

/// Returns `true` if the ring is wound clockwise.
#[inline]
pub fn is_cw(ring: &[Point]) -> bool {
    signed_area(ring) < 0.0
}

/// Reverse a ring's winding order in place.
pub fn reverse_ring(ring: &mut [Point]) {
    ring.reverse();
}

// ---------------------------------------------------------------------------
// Area calculation in square meters (approximate)
// ---------------------------------------------------------------------------

/// Approximate area of a ring in square meters.
///
/// The ring is in Mercator [0,1] space. We scale by Earth's circumference squared
/// and apply a latitude correction for Mercator distortion.
pub fn area_sq_meters(ring: &[Point]) -> f64 {
    let merc_area = signed_area(ring).abs();
    // Find centroid y for latitude correction
    let avg_y = ring.iter().map(|p| p.y).sum::<f64>() / ring.len() as f64;
    let lat = merc_y_to_lat(avg_y);
    let lat_rad = lat * PI / 180.0;
    let cos_lat = lat_rad.cos();
    // In Mercator [0,1], x maps to C meters, y maps to C meters at equator.
    // At latitude lat, x-scale is C * cos(lat), but Mercator y-scale compensates.
    // The actual area is: merc_area * C^2 * cos(lat)^2 approximately.
    // However, Mercator y already stretches by 1/cos(lat), so the y-extent in real
    // meters is roughly C * cos(lat) * (merc_dy / cos(lat)) = C * merc_dy.
    // Net area ≈ merc_area * C^2 is a rough first-order approximation.
    // For better accuracy, we scale by cos²(lat) since both axes in [0,1] span C.
    merc_area * EARTH_CIRCUMFERENCE * EARTH_CIRCUMFERENCE * cos_lat * cos_lat
}

// ---------------------------------------------------------------------------
// Point on surface (for polygon label placement)
// ---------------------------------------------------------------------------

/// Find a point guaranteed to be inside the polygon, suitable for label placement.
///
/// Scans horizontal lines through the polygon bbox, finds the longest interior
/// segment, and returns its midpoint.
pub fn point_on_surface(ring: &[Point]) -> Option<Point> {
    if ring.len() < 3 {
        return None;
    }

    let (bbox_min, bbox_max) = ring_bbox(ring);
    let height = bbox_max.y - bbox_min.y;

    if height < 1e-15 {
        return None;
    }

    let num_scans = 5;
    let mut best_point = None;
    let mut best_length: f64 = 0.0;

    for i in 1..=num_scans {
        let frac = f64::from(i) / f64::from(num_scans + 1);
        let scan_y = bbox_min.y + frac * height;
        scan_for_longest_segment(ring, scan_y, &mut best_point, &mut best_length);
    }

    best_point
}

/// Compute the axis-aligned bounding box of a ring.
fn ring_bbox(ring: &[Point]) -> (Point, Point) {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in ring {
        if p.x < min_x { min_x = p.x; }
        if p.y < min_y { min_y = p.y; }
        if p.x > max_x { max_x = p.x; }
        if p.y > max_y { max_y = p.y; }
    }
    (Point::new(min_x, min_y), Point::new(max_x, max_y))
}

/// Scan a horizontal line at `scan_y` through the ring and update best point/length.
fn scan_for_longest_segment(
    ring: &[Point],
    scan_y: f64,
    best_point: &mut Option<Point>,
    best_length: &mut f64,
) {
    let mut intersections = collect_intersections(ring, scan_y);
    intersections.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    // Walk pairs of intersections (entry/exit)
    let mut i = 0;
    while i + 1 < intersections.len() {
        let x0 = intersections[i];
        let x1 = intersections[i + 1];
        let seg_len = x1 - x0;
        if seg_len > *best_length {
            *best_length = seg_len;
            *best_point = Some(Point::new((x0 + x1) * 0.5, scan_y));
        }
        i += 2;
    }
}

/// Collect x-coordinates where the scan line at `scan_y` intersects ring edges.
fn collect_intersections(ring: &[Point], scan_y: f64) -> Vec<f64> {
    let n = ring.len();
    let mut xs = Vec::new();
    for i in 0..n {
        let j = (i + 1) % n;
        let a = &ring[i];
        let b = &ring[j];
        let (lo_y, hi_y) = if a.y < b.y { (a.y, b.y) } else { (b.y, a.y) };
        // Check if scan line crosses this edge (half-open interval to avoid double-counting vertices)
        if scan_y >= lo_y && scan_y < hi_y {
            let t = (scan_y - a.y) / (b.y - a.y);
            xs.push(a.x + t * (b.x - a.x));
        }
    }
    xs
}

// ---------------------------------------------------------------------------
// Tile intersection
// ---------------------------------------------------------------------------

/// Find all tiles at a given zoom level that intersect the given Mercator bbox.
///
/// Returns a list of `(tile_x, tile_y)` pairs.
pub fn tiles_for_bbox(bbox: &MercBbox, zoom: u8) -> Vec<(u32, u32)> {
    let z_scale = f64::from(1u32 << zoom);
    let max_tile = (1u32 << zoom).saturating_sub(1);

    let tx_min = clamp_tile(bbox.min_x * z_scale, max_tile);
    let tx_max = clamp_tile(bbox.max_x * z_scale, max_tile);
    let ty_min = clamp_tile(bbox.min_y * z_scale, max_tile);
    let ty_max = clamp_tile(bbox.max_y * z_scale, max_tile);

    let mut tiles = Vec::with_capacity(
        ((tx_max - tx_min + 1) * (ty_max - ty_min + 1)) as usize,
    );
    for ty in ty_min..=ty_max {
        for tx in tx_min..=tx_max {
            tiles.push((tx, ty));
        }
    }
    tiles
}

/// Iterate tiles for a bbox without allocating a Vec.
#[inline]
pub fn for_each_tile_in_bbox<F>(bbox: &MercBbox, zoom: u8, mut f: F)
where
    F: FnMut(u32, u32),
{
    let z_scale = f64::from(1u32 << zoom);
    let max_tile = (1u32 << zoom).saturating_sub(1);

    let tx_min = clamp_tile(bbox.min_x * z_scale, max_tile);
    let tx_max = clamp_tile(bbox.max_x * z_scale, max_tile);
    let ty_min = clamp_tile(bbox.min_y * z_scale, max_tile);
    let ty_max = clamp_tile(bbox.max_y * z_scale, max_tile);

    for ty in ty_min..=ty_max {
        for tx in tx_min..=tx_max {
            f(tx, ty);
        }
    }
}

/// Clamp a floating-point tile coordinate to `[0, max_tile]` and floor to u32.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn clamp_tile(val: f64, max_tile: u32) -> u32 {
    let floored = val.floor();
    if floored < 0.0 {
        0
    } else if floored > f64::from(max_tile) {
        max_tile
    } else {
        floored as u32
    }
}

/// Project a WGS84 bbox to Mercator and return the `MercBbox`.
pub fn project_bbox(south: f64, west: f64, north: f64, east: f64) -> MercBbox {
    let sw = project(south, west);
    let ne = project(north, east);
    MercBbox {
        min_x: sw.x,
        min_y: ne.y, // In Mercator, north has smaller y
        max_x: ne.x,
        max_y: sw.y,
    }
}

// ---------------------------------------------------------------------------
// MVT geometry helpers
// ---------------------------------------------------------------------------

/// Convert a slice of Mercator points to tile pixel coordinates for MVT encoding.
#[allow(clippy::cast_possible_truncation)]
pub fn to_tile_coords(
    points: &[Point],
    tile_x: u32,
    tile_y: u32,
    zoom: u8,
) -> Vec<(i32, i32)> {
    let z_scale = f64::from(1u32 << zoom);
    let tx = f64::from(tile_x);
    let ty = f64::from(tile_y);
    points
        .iter()
        .map(|p| {
            let px_x = (p.x * z_scale - tx) * EXTENT;
            let px_y = (p.y * z_scale - ty) * EXTENT;
            (px_x.round() as i32, px_y.round() as i32)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tile-coordinate helpers (shared by tilegen + ocean)
// ---------------------------------------------------------------------------

/// Compute a `MercBbox` bounding box from a slice of Mercator points.
pub(crate) fn merc_bbox(points: &[Point]) -> MercBbox {
    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;
    for p in points {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    MercBbox { min_x, min_y, max_x, max_y }
}

/// Ensure a tile-coordinate ring is clockwise (for MVT outer rings).
/// In tile coordinates, Y increases downward, so positive signed area = CW.
pub(crate) fn ensure_cw_tile(ring: &mut [(i32, i32)]) {
    let area = signed_area_tile(ring);
    if area < 0.0 {
        ring.reverse();
    }
}

/// Ensure a tile-coordinate ring is counter-clockwise (for MVT inner rings).
pub(crate) fn ensure_ccw_tile(ring: &mut [(i32, i32)]) {
    let area = signed_area_tile(ring);
    if area > 0.0 {
        ring.reverse();
    }
}

/// Signed area of a ring in tile coordinates (Shoelace formula).
/// Positive = clockwise in screen coords (Y-down).
fn signed_area_tile(ring: &[(i32, i32)]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut sum: f64 = 0.0;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        sum += f64::from(ring[i].0) * f64::from(ring[j].1);
        sum -= f64::from(ring[j].0) * f64::from(ring[i].1);
    }
    sum / 2.0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
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
}
