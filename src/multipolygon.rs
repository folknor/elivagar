// Multipolygon assembly from OSM relation member ways.
//
// Takes a set of member ways (with roles "outer", "inner", or "") and joins
// them end-to-end into closed rings, then pairs inner rings with their
// containing outer ring to produce complete multipolygon geometry.
//
// All coordinates are in Mercator [0,1] space.

use rustc_hash::FxHashMap;

use crate::geometry::{
    self,
    polygon::{self, RingOrientation},
    projection::Point,
};
use crate::shortbread::PolygonPointStrategy;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Role of a member way in a multipolygon relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WayRole {
    Outer,
    Inner,
    Other,
}

impl WayRole {
    pub fn from_str(s: &str) -> Self {
        match s {
            "outer" => Self::Outer,
            "inner" => Self::Inner,
            _ => Self::Other,
        }
    }
}

/// A member way with role and coordinates (already projected to Mercator).
pub struct MemberWay {
    pub role: WayRole,
    pub coords: Vec<Point>,
}
const _: () = assert!(std::mem::size_of::<MemberWay>() == 32);

/// A single polygon: (outer_ring, inner_rings).
/// Rings do NOT have duplicated closing vertices.
pub type Polygon = (Vec<Point>, Vec<Vec<Point>>);

/// A fully assembled multipolygon.
pub struct MultiPolygon {
    pub polygons: Box<[Polygon]>,
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

/// Assemble a multipolygon from member ways.
///
/// Ways are joined end-to-end by matching endpoints, then classified into
/// outer/inner rings and paired together.
#[cfg(test)]
pub fn assemble(members: &[MemberWay]) -> MultiPolygon {
    assemble_impl(members, false)
}

/// Assemble geographic member ways, unwrapping complete rings before testing
/// containment. Joining in the original coordinates preserves shared endpoints.
pub(crate) fn assemble_wrapped(members: &[MemberWay]) -> MultiPolygon {
    assemble_impl(members, true)
}

/// What one assembly salvaged around, by discard site. Behavior does not
/// depend on it: assembly keeps what it can from a broken relation, and these
/// make the losses observable through the `MULTIPOLYGON` counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AssemblyStats {
    /// Member chains that never closed into a ring, dropped.
    pub unclosed_chains: u64,
    /// Closed rings under 3 distinct vertices, dropped.
    pub degenerate_rings: u64,
    /// Member ways under 2 coordinates, ignored.
    pub short_members: u64,
    /// Inner rings no outer contains, kept as outer shells.
    pub orphan_inners: u64,
}

impl AssemblyStats {
    fn publish(self) {
        use crate::debug::MULTIPOLYGON;
        use std::sync::atomic::Ordering::Relaxed;
        for (counter, value) in [
            (&MULTIPOLYGON.unclosed_chains, self.unclosed_chains),
            (&MULTIPOLYGON.degenerate_rings, self.degenerate_rings),
            (&MULTIPOLYGON.short_members, self.short_members),
            (&MULTIPOLYGON.orphan_inners, self.orphan_inners),
        ] {
            if value > 0 {
                counter.fetch_add(value, Relaxed);
            }
        }
    }
}

fn assemble_impl(members: &[MemberWay], wrapped: bool) -> MultiPolygon {
    let (multi, stats) = assemble_with_stats(members, wrapped);
    stats.publish();
    multi
}

pub(crate) fn assemble_with_stats(
    members: &[MemberWay],
    wrapped: bool,
) -> (MultiPolygon, AssemblyStats) {
    let mut stats = AssemblyStats::default();
    let (outer_ways, inner_ways, unclassified_ways) = separate_by_role(members, &mut stats);

    let mut outer_rings = join_ways(&outer_ways, &mut stats);
    let mut inner_rings = join_ways(&inner_ways, &mut stats);
    if wrapped {
        for ring in outer_rings.iter_mut().chain(inner_rings.iter_mut()) {
            unwrap_ring(ring);
        }
    }

    classify_unclassified(
        &unclassified_ways,
        &mut outer_rings,
        &mut inner_rings,
        wrapped,
        &mut stats,
    );

    ensure_outer_orientation(&mut outer_rings);
    ensure_inner_orientation(&mut inner_rings);
    sort_rings_deterministic(&mut outer_rings);
    sort_rings_deterministic(&mut inner_rings);

    let mut polygons = pair_rings(outer_rings, inner_rings, wrapped, &mut stats);
    for (_, inners) in &mut polygons {
        sort_rings_deterministic(inners);
    }
    polygons.sort_by_key(|(outer, _)| ring_sort_key(outer));
    let polygons = polygons.into_boxed_slice();
    (MultiPolygon { polygons }, stats)
}

/// Place one label for an assembled relation, treating all polygon components
/// as one source geometry.
pub(crate) fn relation_label_point(
    polygons: &[Polygon],
    strategy: PolygonPointStrategy,
) -> Option<Point> {
    match strategy {
        PolygonPointStrategy::Centroid => aggregate_centroid(polygons),
        PolygonPointStrategy::CentroidIfConvex => {
            // A single-shell relation is a Polygon in Planetiler too. Preserve
            // the convex shortcut for that case; multiple shells use a surface point.
            if let [(outer, inners)] = polygons
                && inners.is_empty()
                && geometry::surface::is_convex(outer)
            {
                geometry::surface::polygon_centroid(outer, inners)
            } else {
                aggregate_point_on_surface(polygons)
            }
        }
        PolygonPointStrategy::PointOnSurface => aggregate_point_on_surface(polygons),
    }
}

fn aggregate_centroid(polygons: &[Polygon]) -> Option<Point> {
    if let [polygon] = polygons {
        return geometry::surface::polygon_centroid(&polygon.0, &polygon.1);
    }
    let reference_x = polygons
        .iter()
        .filter_map(|(outer, inners)| {
            let (area, centroid) = geometry::surface::polygon_area_centroid(outer, inners)?;
            Some((area, centroid.x))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, x)| x)?;
    let mut total_area = 0.0;
    let mut weighted_x = 0.0;
    let mut weighted_y = 0.0;

    for (outer, inners) in polygons {
        let Some((area, centroid)) = geometry::surface::polygon_area_centroid(outer, inners) else {
            continue;
        };
        // Components crossing the antimeridian can occupy adjacent world
        // copies. Align each one to the largest component before averaging.
        let x = centroid.x + (reference_x - centroid.x).round();
        total_area += area;
        weighted_x += x * area;
        weighted_y += centroid.y * area;
    }

    if !total_area.is_finite()
        || total_area <= 0.0
        || !weighted_x.is_finite()
        || !weighted_y.is_finite()
    {
        return None;
    }
    let point = Point::new(weighted_x / total_area, weighted_y / total_area);
    (point.x.is_finite() && point.y.is_finite()).then_some(point)
}

fn aggregate_point_on_surface(polygons: &[Polygon]) -> Option<Point> {
    let mut best: Option<(f64, Point)> = None;
    for (outer, inners) in polygons {
        let Some(area) = planar_area(outer, inners) else {
            continue;
        };
        let Some(point) = geometry::surface::point_on_surface_with_holes(outer, inners) else {
            continue;
        };
        if best.is_none_or(|(best_area, _)| area > best_area) {
            best = Some((area, point));
        }
    }
    best.map(|(_, point)| point)
}

fn planar_area(outer: &[Point], inners: &[Vec<Point>]) -> Option<f64> {
    geometry::surface::polygon_area_centroid(outer, inners).map(|(area, _)| area)
}

fn unwrap_ring(ring: &mut [Point]) {
    for i in 1..ring.len() {
        let previous = ring[i - 1].x;
        while ring[i].x - previous > 0.5 {
            ring[i].x -= 1.0;
        }
        while previous - ring[i].x > 0.5 {
            ring[i].x += 1.0;
        }
    }
}

/// A hole and its shell can start in different world copies. Choose a copy
/// relative to each candidate shell, rather than to an unrelated first member.
fn containment_shift(outer: &[Point], inner: &[Point], wrapped: bool) -> Option<f64> {
    let shift = if wrapped {
        let outer_bbox = geometry::tiles::merc_bbox(outer);
        let inner_bbox = geometry::tiles::merc_bbox(inner);
        ((outer_bbox.min_x + outer_bbox.max_x - inner_bbox.min_x - inner_bbox.max_x) * 0.5).round()
    } else {
        0.0
    };
    ring_contains_ring(outer, inner, shift).then_some(shift)
}

/// Separate member ways into outer, inner, and unclassified groups.
///
/// Skips ways with fewer than 2 coordinates.
type RingGroups<'a> = (Vec<&'a [Point]>, Vec<&'a [Point]>, Vec<&'a [Point]>);

fn separate_by_role<'a>(members: &'a [MemberWay], stats: &mut AssemblyStats) -> RingGroups<'a> {
    let mut outers = Vec::with_capacity(members.len());
    let mut inners = Vec::with_capacity(members.len());
    let mut unclassified = Vec::with_capacity(members.len());

    for m in members {
        if m.coords.len() < 2 {
            stats.short_members += 1;
            continue;
        }
        match m.role {
            WayRole::Outer => outers.push(m.coords.as_slice()),
            WayRole::Inner => inners.push(m.coords.as_slice()),
            WayRole::Other => unclassified.push(m.coords.as_slice()),
        }
    }

    (outers, inners, unclassified)
}

/// Infer missing roles from containment depth, independent of way direction.
/// Explicit roles remain authoritative, but their rings contribute to depth.
fn classify_unclassified(
    unclassified_ways: &[&[Point]],
    outer_rings: &mut Vec<Vec<Point>>,
    inner_rings: &mut Vec<Vec<Point>>,
    wrapped: bool,
    stats: &mut AssemblyStats,
) {
    let mut unc_rings = join_ways(unclassified_ways, stats);
    if wrapped {
        for ring in &mut unc_rings {
            unwrap_ring(ring);
        }
    }
    let depths: Vec<usize> = unc_rings
        .iter()
        .enumerate()
        .map(|(i, ring)| {
            let explicit_depth = outer_rings
                .iter()
                .chain(inner_rings.iter())
                .filter(|outer| containment_shift(outer, ring, wrapped).is_some())
                .count();
            let inferred_depth = unc_rings
                .iter()
                .enumerate()
                .filter(|(j, outer)| i != *j && containment_shift(outer, ring, wrapped).is_some())
                .count();
            explicit_depth + inferred_depth
        })
        .collect();
    for (ring, depth) in unc_rings.into_iter().zip(depths) {
        if depth % 2 == 0 {
            outer_rings.push(ring);
        } else {
            inner_rings.push(ring);
        }
    }
}

/// Boundary points provide no containment evidence. Find a vertex or edge
/// midpoint off the shell, then use it to decide containment. For noncrossing
/// OSM rings one such point suffices; scanning every inner vertex against the
/// entire outer would be quadratic in ring size. Repairing intersecting or
/// coincident contours belongs to topology normalization.
fn ring_contains_ring(outer: &[Point], inner: &[Point], shift: f64) -> bool {
    if outer.len() < 3 || inner.len() < 3 {
        return false;
    }
    for (a, b) in inner.iter().zip(inner.iter().cycle().skip(1)) {
        for point in [
            Point::new(a.x + shift, a.y),
            Point::new((a.x + b.x) * 0.5 + shift, (a.y + b.y) * 0.5),
        ] {
            let boundary = outer
                .iter()
                .zip(outer.iter().cycle().skip(1))
                .any(|(c, d)| {
                    let left = (point.x - c.x) * (d.y - c.y);
                    let right = (point.y - c.y) * (d.x - c.x);
                    let tolerance = 8.0 * f64::EPSILON * (left.abs() + right.abs());
                    (left - right).abs() <= tolerance
                        && point.x >= c.x.min(d.x)
                        && point.x <= c.x.max(d.x)
                        && point.y >= c.y.min(d.y)
                        && point.y <= c.y.max(d.y)
                });
            if boundary {
                continue;
            }
            return geometry::polygon::point_in_polygon(&point, outer);
        }
    }
    false
}

/// Ensure all outer rings have positive signed_area (CW on screen).
fn ensure_outer_orientation(rings: &mut [Vec<Point>]) {
    for ring in rings.iter_mut() {
        if polygon::ring_role(polygon::signed_area(ring)) == Some(RingOrientation::Negative) {
            ring.reverse();
        }
    }
}

/// Ensure all inner rings have negative signed_area (CCW on screen).
fn ensure_inner_orientation(rings: &mut [Vec<Point>]) {
    for ring in rings.iter_mut() {
        if polygon::ring_role(polygon::signed_area(ring)) == Some(RingOrientation::Positive) {
            ring.reverse();
        }
    }
}

/// Pair inner rings with their containing outer ring using point-in-polygon.
fn pair_rings(
    outer_rings: Vec<Vec<Point>>,
    inner_rings: Vec<Vec<Point>>,
    wrapped: bool,
    stats: &mut AssemblyStats,
) -> Vec<(Vec<Point>, Vec<Vec<Point>>)> {
    if outer_rings.is_empty() {
        return Vec::new();
    }

    let mut polygons: Vec<(Vec<Point>, Vec<Vec<Point>>)> =
        outer_rings.into_iter().map(|r| (r, Vec::new())).collect();

    let outer_count = polygons.len();
    for mut inner in inner_rings {
        if inner.is_empty() {
            continue;
        }
        // For nested shells, the smallest containing shell is the immediate
        // parent. Restrict candidates to the original outers so orphan
        // promotion cannot make pairing depend on inner-ring iteration order.
        let target = polygons[..outer_count]
            .iter()
            .enumerate()
            .filter_map(|(i, poly)| {
                containment_shift(&poly.0, &inner, wrapped)
                    .map(|shift| (i, shift, polygon::signed_area(&poly.0).abs()))
            })
            .min_by(|a, b| a.2.total_cmp(&b.2));
        if let Some((idx, shift, _)) = target {
            if wrapped {
                for point in &mut inner {
                    point.x += shift;
                }
            }
            polygons[idx].1.push(inner);
        } else {
            // Orphan inner ring: promote to an oriented outer shell.
            stats.orphan_inners += 1;
            inner.reverse();
            normalize_ring_start(&mut inner);
            polygons.push((inner, Vec::new()));
        }
    }

    polygons
}

// ---------------------------------------------------------------------------
// Way joining
// ---------------------------------------------------------------------------

/// Quantize a point to an i64 pair for use as a HashMap key.
///
/// Uses 1e-9 precision to avoid float comparison issues.
#[allow(clippy::cast_possible_truncation)]
fn quantize(p: &Point) -> (i64, i64) {
    // Mercator [0,1] coordinates multiplied by 1e9 fit comfortably in i64.
    let x = (p.x * 1e9).round() as i64;
    let y = (p.y * 1e9).round() as i64;
    (x, y)
}

/// Join a list of coordinate sequences end-to-end to form closed rings.
///
/// Returns the closed rings. Chains that never close are dropped and counted
/// in `stats`, as are closed rings too short to carry area.
///
/// Two-pass algorithm: the first pass greedily appends ways to chains via an
/// endpoint map. This can leave orphaned chains when `endpoint_map.insert()`
/// overwrites entries for other chains. A second pass rebuilds the map from
/// surviving chains and attempts pairwise joins until no more progress is made.
#[allow(clippy::too_many_lines)]
fn join_ways(ways: &[&[Point]], stats: &mut AssemblyStats) -> Vec<Vec<Point>> {
    let mut chains: Vec<Vec<Point>> = Vec::with_capacity(ways.len());
    // Maps an endpoint (quantized) to the index in `chains` that has that endpoint.
    let mut endpoint_map: FxHashMap<(i64, i64), usize> = FxHashMap::default();
    let mut closed = ClosedRings {
        rings: Vec::with_capacity(ways.len()),
        degenerate: 0,
    };

    for way in ways {
        if way.len() < 2 {
            // separate_by_role already counted and removed these.
            continue;
        }
        // Check if this single way is already a closed ring on its own.
        if quantize(&way[0]) == quantize(&way[way.len() - 1]) {
            let mut ring = way.to_vec();
            ring.pop(); // Remove duplicated closing vertex.
            closed.push(ring);
            continue;
        }
        append_way_to_chains(way, &mut chains, &mut endpoint_map, &mut closed);
    }

    // Second pass: merge unclosed chains pairwise until no more progress.
    loop {
        let mut merged_any = false;
        endpoint_map.clear();
        for (i, chain) in chains.iter().enumerate() {
            if chain.len() < 2 {
                continue;
            }
            endpoint_map.insert(quantize(&chain[0]), i);
            endpoint_map.insert(quantize(&chain[chain.len() - 1]), i);
        }

        // Try to join each unclosed chain with another via shared endpoints.
        let chain_count = chains.len();
        for i in 0..chain_count {
            if chains[i].len() < 2 {
                continue;
            }
            let front = quantize(&chains[i][0]);
            let back = quantize(&chains[i][chains[i].len() - 1]);

            // Look for another chain matching our back endpoint.
            if let Some(&j) = endpoint_map.get(&back)
                && j != i
                && chains[j].len() >= 2
            {
                let (j_front, j_back) = (
                    quantize(&chains[j][0]),
                    quantize(&chains[j][chains[j].len() - 1]),
                );
                let taken_j = std::mem::take(&mut chains[j]);
                if back == j_front {
                    chains[i].extend_from_slice(&taken_j[1..]);
                } else if back == j_back {
                    chains[i].extend(taken_j.iter().rev().skip(1).copied());
                } else {
                    chains[j] = taken_j;
                    continue;
                }
                let new_front = quantize(&chains[i][0]);
                let new_back = quantize(&chains[i][chains[i].len() - 1]);
                if new_front == new_back {
                    let mut ring = std::mem::take(&mut chains[i]);
                    ring.pop();
                    closed.push(ring);
                }
                merged_any = true;
                break;
            }

            // Look for another chain matching our front endpoint.
            if let Some(&j) = endpoint_map.get(&front)
                && j != i
                && chains[j].len() >= 2
            {
                let (j_front, j_back) = (
                    quantize(&chains[j][0]),
                    quantize(&chains[j][chains[j].len() - 1]),
                );
                let taken_j = std::mem::take(&mut chains[j]);
                if front == j_back {
                    let mut merged = taken_j;
                    merged.extend_from_slice(&chains[i][1..]);
                    chains[i] = merged;
                } else if front == j_front {
                    let mut merged =
                        Vec::with_capacity(taken_j.len() + chains[i].len().saturating_sub(1));
                    merged.extend(taken_j.iter().rev().copied());
                    merged.extend_from_slice(&chains[i][1..]);
                    chains[i] = merged;
                } else {
                    chains[j] = taken_j;
                    continue;
                }
                let new_front = quantize(&chains[i][0]);
                let new_back = quantize(&chains[i][chains[i].len() - 1]);
                if new_front == new_back {
                    let mut ring = std::mem::take(&mut chains[i]);
                    ring.pop();
                    closed.push(ring);
                }
                merged_any = true;
                break;
            }
        }

        if !merged_any {
            break;
        }
    }

    // Remaining chains never closed. Slots emptied by a join are not chains.
    let unclosed = chains.iter().filter(|c| c.len() >= 2).count();
    stats.unclosed_chains += u64::try_from(unclosed).unwrap_or(u64::MAX);
    stats.degenerate_rings += closed.degenerate;
    closed.rings
}

/// Closed rings under assembly, with the closures too short to keep.
struct ClosedRings {
    rings: Vec<Vec<Point>>,
    degenerate: u64,
}

impl ClosedRings {
    /// Keep a closed ring (closing vertex already removed) if it has the 3
    /// vertices a ring needs; count it otherwise.
    fn push(&mut self, ring: Vec<Point>) {
        if ring.len() >= 3 {
            self.rings.push(ring);
        } else {
            self.degenerate += 1;
        }
    }
}

/// Try to attach a single way to an existing chain, or start a new chain.
///
/// When a chain becomes closed, it is moved to `closed`.
fn append_way_to_chains(
    way: &[Point],
    chains: &mut Vec<Vec<Point>>,
    endpoint_map: &mut FxHashMap<(i64, i64), usize>,
    closed: &mut ClosedRings,
) {
    let way_front = quantize(&way[0]);
    let way_back = quantize(&way[way.len() - 1]);

    // Try to find a chain endpoint matching our front.
    if let Some(&idx) = endpoint_map.get(&way_front)
        && idx < chains.len()
        && !chains[idx].is_empty()
    {
        attach_way(idx, way, chains, endpoint_map, closed);
        return;
    }

    // Try to find a chain endpoint matching our back.
    if let Some(&idx) = endpoint_map.get(&way_back)
        && idx < chains.len()
        && !chains[idx].is_empty()
    {
        let mut reversed: Vec<Point> = way.to_vec();
        reversed.reverse();
        attach_way(idx, &reversed, chains, endpoint_map, closed);
        return;
    }

    // No match -- start a new chain.
    let new_idx = chains.len();
    chains.push(way.to_vec());
    endpoint_map.insert(way_front, new_idx);
    endpoint_map.insert(way_back, new_idx);
}

/// Attach `way` to the chain at `idx`, joining at the matching endpoint.
///
/// Handles all four orientation cases (front-to-back, front-to-front, etc.)
/// and checks for ring closure after joining.
fn attach_way(
    idx: usize,
    way: &[Point],
    chains: &mut Vec<Vec<Point>>,
    endpoint_map: &mut FxHashMap<(i64, i64), usize>,
    closed: &mut ClosedRings,
) {
    let chain_front = quantize(&chains[idx][0]);
    let chain_back = quantize(&chains[idx][chains[idx].len() - 1]);
    let way_front = quantize(&way[0]);
    let way_back = quantize(&way[way.len() - 1]);

    if chain_back == way_front {
        // Append way to end of chain (skip first point to avoid duplication).
        endpoint_map.remove(&chain_back);
        chains[idx].extend_from_slice(&way[1..]);
    } else if chain_front == way_back {
        // Prepend way to front of chain.
        endpoint_map.remove(&chain_front);
        let mut new_chain = way.to_vec();
        new_chain.extend_from_slice(&chains[idx][1..]);
        chains[idx] = new_chain;
    } else if chain_front == way_front {
        // Prepend reversed way to front of chain.
        endpoint_map.remove(&chain_front);
        let mut new_chain: Vec<Point> =
            Vec::with_capacity(way.len() + chains[idx].len().saturating_sub(1));
        new_chain.extend(way.iter().rev().copied());
        new_chain.extend_from_slice(&chains[idx][1..]);
        chains[idx] = new_chain;
    } else if chain_back == way_back {
        // Append reversed way to end of chain.
        endpoint_map.remove(&chain_back);
        chains[idx].extend(way.iter().rev().skip(1).copied());
    } else {
        // Endpoint map was stale -- start a new chain.
        start_new_chain(way, chains, endpoint_map);
        return;
    }

    finalize_chain(idx, chains, endpoint_map, closed);
}

/// Start a new chain from `way` and register its endpoints.
fn start_new_chain(
    way: &[Point],
    chains: &mut Vec<Vec<Point>>,
    endpoint_map: &mut FxHashMap<(i64, i64), usize>,
) {
    let new_idx = chains.len();
    chains.push(way.to_vec());
    let front = quantize(&way[0]);
    let back = quantize(&way[way.len() - 1]);
    endpoint_map.insert(front, new_idx);
    endpoint_map.insert(back, new_idx);
}

/// Check if chain at `idx` is now closed. If so, move it to `closed`.
/// Otherwise update the endpoint map with new endpoints.
fn finalize_chain(
    idx: usize,
    chains: &mut [Vec<Point>],
    endpoint_map: &mut FxHashMap<(i64, i64), usize>,
    closed: &mut ClosedRings,
) {
    let new_front = quantize(&chains[idx][0]);
    let new_back = quantize(&chains[idx][chains[idx].len() - 1]);

    if new_front == new_back {
        // Chain is closed -- extract it as a ring.
        endpoint_map.remove(&new_front);
        endpoint_map.remove(&new_back);
        let mut ring = std::mem::take(&mut chains[idx]);
        ring.pop(); // Remove duplicated closing vertex.
        closed.push(ring);
    } else {
        endpoint_map.insert(new_front, idx);
        endpoint_map.insert(new_back, idx);
    }
}

fn ring_sort_key(ring: &[Point]) -> (i64, i64, i64, i64, usize) {
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    for p in ring {
        let (x, y) = quantize(p);
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    (min_x, min_y, max_x, max_y, ring.len())
}

fn compare_ring_rotations(ring: &[Point], i: usize, j: usize) -> std::cmp::Ordering {
    let n = ring.len();
    for k in 0..n {
        let a = quantize(&ring[(i + k) % n]);
        let b = quantize(&ring[(j + k) % n]);
        let ord = a.cmp(&b);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

fn normalize_ring_start(ring: &mut [Point]) {
    if ring.len() < 2 {
        return;
    }
    let mut best = 0usize;
    for i in 1..ring.len() {
        if compare_ring_rotations(ring, i, best) == std::cmp::Ordering::Less {
            best = i;
        }
    }
    if best > 0 {
        ring.rotate_left(best);
    }
}

fn compare_rings_lex(a: &[Point], b: &[Point]) -> std::cmp::Ordering {
    let n = a.len().min(b.len());
    for i in 0..n {
        let ord = quantize(&a[i]).cmp(&quantize(&b[i]));
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

fn sort_rings_deterministic(rings: &mut [Vec<Point>]) {
    for ring in rings.iter_mut() {
        normalize_ring_start(ring);
    }
    rings.sort_by(|a, b| {
        ring_sort_key(a)
            .cmp(&ring_sort_key(b))
            .then_with(|| compare_rings_lex(a, b))
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn pt(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    fn make_member(role: &str, coords: Vec<Point>) -> MemberWay {
        MemberWay {
            role: WayRole::from_str(role),
            coords,
        }
    }

    fn ring_keys(ring: &[Point]) -> Vec<(i64, i64)> {
        ring.iter().map(quantize).collect()
    }

    // All regression geometry fits inside the z0 tile.
    fn square(lo: f64, hi: f64) -> Vec<Point> {
        vec![pt(lo, lo), pt(hi, lo), pt(hi, hi), pt(lo, hi), pt(lo, lo)]
    }

    fn wrapped_rectangle(lo: f64, hi: f64, y0: f64, y1: f64) -> Vec<Point> {
        vec![
            pt(lo.rem_euclid(1.0), y0),
            pt(hi.rem_euclid(1.0), y0),
            pt(hi.rem_euclid(1.0), y1),
            pt(lo.rem_euclid(1.0), y1),
            pt(lo.rem_euclid(1.0), y0),
        ]
    }

    #[test]
    fn wrapped_assembly_pairs_holes_relative_to_each_shell() {
        for role in ["inner", ""] {
            for (lo, hi, hole_lo, hole_hi) in [(0.59, 0.61, 0.601, 0.609), (0.98, 1.02, 0.99, 1.01)]
            {
                for reverse in [false, true] {
                    let mut hole = wrapped_rectangle(hole_lo, hole_hi, 0.45, 0.55);
                    if reverse {
                        hole.reverse();
                    }
                    let mp = assemble_wrapped(&[
                        make_member("outer", wrapped_rectangle(0.1, 0.12, 0.1, 0.12)),
                        make_member("outer", wrapped_rectangle(lo, hi, 0.4, 0.6)),
                        make_member(role, hole),
                    ]);
                    assert_eq!(mp.polygons.len(), 2);
                    assert_eq!(
                        mp.polygons
                            .iter()
                            .map(|(_, holes)| holes.len())
                            .sum::<usize>(),
                        1
                    );
                    for (shell, holes) in &mp.polygons {
                        assert!(
                            holes
                                .iter()
                                .all(|hole| ring_contains_ring(shell, hole, 0.0))
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn wrapped_assembly_joins_members_before_unwrapping() {
        let mp = assemble_wrapped(&[
            make_member("outer", vec![pt(0.1, 0.4), pt(0.4, 0.4), pt(0.7, 0.4)]),
            make_member(
                "outer",
                vec![
                    pt(0.7, 0.4),
                    pt(0.7, 0.6),
                    pt(0.4, 0.6),
                    pt(0.1, 0.6),
                    pt(0.1, 0.4),
                ],
            ),
        ]);
        assert_eq!(mp.polygons.len(), 1);
        assert_eq!(mp.polygons[0].0.len(), 6);
    }

    #[test]
    fn touching_hole_survives_ring_rotation_and_reversal() {
        for reverse in [false, true] {
            // The rightmost boundary is outside under the ray-casting rule.
            let mut hole = vec![pt(0.875, 0.5), pt(0.625, 0.625), pt(0.625, 0.375)];
            if reverse {
                hole.reverse();
            }
            for rotation in 0..hole.len() {
                let mut rotated = hole.clone();
                rotated.rotate_left(rotation);
                rotated.push(rotated[0]);
                let mp = assemble(&[
                    make_member("outer", square(0.125, 0.875)),
                    make_member("inner", rotated),
                ]);
                assert_eq!(mp.polygons.len(), 1);
                assert_eq!(mp.polygons[0].1.len(), 1);
            }
        }
    }

    #[test]
    fn hole_with_all_vertices_on_shell_uses_edge_interiors() {
        let mp = assemble(&[
            make_member("outer", square(0.125, 0.875)),
            make_member(
                "inner",
                vec![
                    pt(0.125, 0.5),
                    pt(0.5, 0.125),
                    pt(0.875, 0.5),
                    pt(0.5, 0.875),
                    pt(0.125, 0.5),
                ],
            ),
        ]);
        assert_eq!(mp.polygons.len(), 1);
        assert_eq!(mp.polygons[0].1.len(), 1);
    }

    #[test]
    fn nested_hole_pairs_with_smallest_shell() {
        let mp = assemble(&[
            make_member("outer", square(0.0625, 0.9375)),
            make_member("inner", square(0.125, 0.875)),
            make_member("outer", square(0.25, 0.75)),
            make_member("inner", square(0.375, 0.625)),
        ]);
        assert_eq!(mp.polygons.len(), 2);
        assert!(mp.polygons.iter().all(|(_, holes)| holes.len() == 1));
        assert!(
            polygon::signed_area(&mp.polygons[1].1[0]).abs()
                < polygon::signed_area(&mp.polygons[0].1[0]).abs()
        );
    }

    #[test]
    fn missing_roles_ignore_direction_and_member_order() {
        for mask in 0..16 {
            let mut members = Vec::new();
            for (i, (lo, hi)) in [
                (0.0625, 0.9375),
                (0.125, 0.875),
                (0.25, 0.75),
                (0.375, 0.625),
            ]
            .into_iter()
            .enumerate()
            {
                let mut ring = square(lo, hi);
                if mask & (1 << i) != 0 {
                    ring.reverse();
                }
                members.push(make_member("", ring));
            }
            if mask % 2 == 0 {
                members.reverse();
            }
            let mp = assemble(&members);
            assert_eq!(mp.polygons.len(), 2);
            assert!(mp.polygons.iter().all(|(_, holes)| holes.len() == 1));
        }
    }

    #[test]
    fn missing_role_depth_includes_explicit_rings() {
        let mp = assemble(&[
            make_member("outer", square(0.0625, 0.9375)),
            make_member("", square(0.125, 0.875)),
            make_member("outer", square(0.25, 0.75)),
            make_member("", square(0.375, 0.625)),
        ]);
        assert_eq!(mp.polygons.len(), 2);
        assert!(mp.polygons.iter().all(|(_, holes)| holes.len() == 1));
    }

    #[test]
    fn salvage_is_counted_at_every_discard_site() {
        let open_chain = vec![pt(0.5, 0.5), pt(0.6, 0.5), pt(0.6, 0.6)];
        let (mp, stats) = assemble_with_stats(
            &[
                make_member("outer", square(0.0, 0.1)),
                make_member("outer", open_chain.clone()),
                make_member("outer", vec![pt(0.7, 0.7)]),
                make_member("outer", vec![pt(0.8, 0.8), pt(0.9, 0.8), pt(0.8, 0.8)]),
                make_member("inner", square(0.3, 0.32)),
                make_member("", open_chain),
            ],
            false,
        );
        assert_eq!(
            stats,
            AssemblyStats {
                unclosed_chains: 2,
                degenerate_rings: 1,
                short_members: 1,
                orphan_inners: 1,
            }
        );
        // Salvage itself is unchanged: the shell and the promoted orphan.
        assert_eq!(mp.polygons.len(), 2);
    }

    #[test]
    fn clean_assembly_counts_nothing() {
        let (_, stats) = assemble_with_stats(
            &[
                make_member("outer", square(0.0, 0.4)),
                make_member("inner", square(0.1, 0.2)),
            ],
            false,
        );
        assert_eq!(stats, AssemblyStats::default());
    }

    #[test]
    fn promoted_orphan_has_shell_orientation() {
        let mp = assemble(&[
            make_member("outer", square(0.0625, 0.25)),
            make_member("inner", square(0.5, 0.75)),
        ]);
        assert_eq!(mp.polygons.len(), 2);
        assert!(
            mp.polygons
                .iter()
                .all(|(shell, _)| polygon::signed_area(shell) > 0.0)
        );
    }

    // --- point_in_polygon ---

    #[test]
    fn test_point_in_polygon_inside() {
        let ring = vec![pt(0.0, 0.0), pt(1.0, 0.0), pt(1.0, 1.0), pt(0.0, 1.0)];
        assert!(geometry::polygon::point_in_polygon(&pt(0.5, 0.5), &ring));
    }

    #[test]
    fn test_point_in_polygon_outside() {
        let ring = vec![pt(0.0, 0.0), pt(1.0, 0.0), pt(1.0, 1.0), pt(0.0, 1.0)];
        assert!(!geometry::polygon::point_in_polygon(&pt(2.0, 2.0), &ring));
    }

    #[test]
    fn test_point_in_polygon_degenerate() {
        let ring = vec![pt(0.0, 0.0), pt(1.0, 0.0)];
        assert!(!geometry::polygon::point_in_polygon(&pt(0.5, 0.0), &ring));
    }

    #[test]
    fn test_point_in_polygon_triangle() {
        let ring = vec![pt(0.0, 0.0), pt(2.0, 0.0), pt(1.0, 2.0)];
        assert!(geometry::polygon::point_in_polygon(&pt(1.0, 0.5), &ring));
        assert!(!geometry::polygon::point_in_polygon(&pt(0.0, 2.0), &ring));
    }

    // --- Single outer ring from 3 ways joining end-to-end ---

    #[test]
    fn test_three_ways_join_into_outer() {
        // A triangle split into three ways:
        //   way1: (0,0) -> (1,0)
        //   way2: (1,0) -> (0.5, 1)
        //   way3: (0.5, 1) -> (0,0)
        let members = vec![
            make_member("outer", vec![pt(0.0, 0.0), pt(1.0, 0.0)]),
            make_member("outer", vec![pt(1.0, 0.0), pt(0.5, 1.0)]),
            make_member("outer", vec![pt(0.5, 1.0), pt(0.0, 0.0)]),
        ];

        let mp = assemble(&members);
        assert_eq!(mp.polygons.len(), 1, "should produce one polygon");
        let (outer, inners) = &mp.polygons[0];
        assert_eq!(
            outer.len(),
            3,
            "triangle ring should have 3 vertices (no closing dup)",
        );
        assert!(inners.is_empty(), "should have no inner rings");
    }

    // --- Outer + inner (hole) correctly paired ---

    #[test]
    fn test_outer_with_inner_hole() {
        // Outer: large square
        let members = vec![
            make_member(
                "outer",
                vec![
                    pt(0.0, 0.0),
                    pt(10.0, 0.0),
                    pt(10.0, 10.0),
                    pt(0.0, 10.0),
                    pt(0.0, 0.0),
                ],
            ),
            // Inner: small square inside the outer
            make_member(
                "inner",
                vec![
                    pt(2.0, 2.0),
                    pt(8.0, 2.0),
                    pt(8.0, 8.0),
                    pt(2.0, 8.0),
                    pt(2.0, 2.0),
                ],
            ),
        ];

        let mp = assemble(&members);
        assert_eq!(mp.polygons.len(), 1, "should produce one polygon");
        let (outer, inners) = &mp.polygons[0];
        assert!(
            outer.len() >= 3,
            "outer ring should have at least 3 vertices",
        );
        assert_eq!(inners.len(), 1, "should have one inner ring (hole)");
        assert!(
            inners[0].len() >= 3,
            "inner ring should have at least 3 vertices",
        );
    }

    // --- Two disjoint outer rings ---

    #[test]
    fn test_two_disjoint_outers() {
        let members = vec![
            make_member(
                "outer",
                vec![
                    pt(0.0, 0.0),
                    pt(1.0, 0.0),
                    pt(1.0, 1.0),
                    pt(0.0, 1.0),
                    pt(0.0, 0.0),
                ],
            ),
            make_member(
                "outer",
                vec![
                    pt(5.0, 5.0),
                    pt(6.0, 5.0),
                    pt(6.0, 6.0),
                    pt(5.0, 6.0),
                    pt(5.0, 5.0),
                ],
            ),
        ];

        let mp = assemble(&members);
        assert_eq!(mp.polygons.len(), 2, "should produce two polygons");
        for (outer, inners) in &mp.polygons {
            assert!(
                outer.len() >= 3,
                "each outer ring should have at least 3 vertices",
            );
            assert!(inners.is_empty(), "should have no inner rings");
        }
    }

    // --- Unclassified roles resolved by containment ---

    #[test]
    fn test_unclassified_resolved_by_containment() {
        // The top-level ring is a shell, regardless of winding.
        let members = vec![
            make_member(
                "",
                vec![
                    pt(0.0, 0.0),
                    pt(10.0, 0.0),
                    pt(10.0, 10.0),
                    pt(0.0, 10.0),
                    pt(0.0, 0.0),
                ],
            ),
            // One containing ring makes this a hole.
            make_member(
                "",
                vec![
                    pt(3.0, 3.0),
                    pt(3.0, 7.0),
                    pt(7.0, 7.0),
                    pt(7.0, 3.0),
                    pt(3.0, 3.0),
                ],
            ),
        ];

        let mp = assemble(&members);
        assert_eq!(mp.polygons.len(), 1, "should produce one polygon");
        let (_, inners) = &mp.polygons[0];
        assert_eq!(inners.len(), 1, "should have one inner ring");
    }

    // --- Degenerate input (empty ways) handled gracefully ---

    #[test]
    fn test_degenerate_empty_ways() {
        let members = vec![
            make_member("outer", vec![]),
            make_member("outer", vec![pt(0.0, 0.0)]),
            make_member("inner", vec![]),
        ];

        let mp = assemble(&members);
        assert!(
            mp.polygons.is_empty(),
            "degenerate input should produce empty multipolygon",
        );
    }

    // --- No members at all ---

    #[test]
    fn test_empty_members() {
        let mp = assemble(&[]);
        assert!(mp.polygons.is_empty());
    }

    // --- Reversed ways are still joined ---

    #[test]
    fn test_reversed_ways_joined() {
        // way1: (0,0) -> (1,0)
        // way2 is reversed: (0.5,1) -> (1,0)
        // way3: (0.5,1) -> (0,0)
        let members = vec![
            make_member("outer", vec![pt(0.0, 0.0), pt(1.0, 0.0)]),
            make_member("outer", vec![pt(0.5, 1.0), pt(1.0, 0.0)]),
            make_member("outer", vec![pt(0.5, 1.0), pt(0.0, 0.0)]),
        ];

        let mp = assemble(&members);
        assert_eq!(mp.polygons.len(), 1, "reversed ways should still join");
        assert!(mp.polygons[0].0.len() >= 3);
    }

    // --- Ring orientation enforcement ---

    #[test]
    fn test_outer_ring_orientation_enforced() {
        // CW ring in math coords (negative signed_area) -- should be flipped.
        let members = vec![make_member(
            "outer",
            vec![
                pt(0.0, 1.0),
                pt(1.0, 1.0),
                pt(1.0, 0.0),
                pt(0.0, 0.0),
                pt(0.0, 1.0),
            ],
        )];

        let mp = assemble(&members);
        assert_eq!(mp.polygons.len(), 1);
        let area = polygon::signed_area(&mp.polygons[0].0);
        assert!(
            area > 0.0,
            "outer ring should have positive signed_area, got {area}",
        );
    }

    #[test]
    fn test_inner_ring_orientation_enforced() {
        // Outer: CCW (positive area) -- will stay positive.
        // Inner: also CCW (positive area) -- should be flipped to negative.
        let members = vec![
            make_member(
                "outer",
                vec![
                    pt(0.0, 0.0),
                    pt(10.0, 0.0),
                    pt(10.0, 10.0),
                    pt(0.0, 10.0),
                    pt(0.0, 0.0),
                ],
            ),
            make_member(
                "inner",
                vec![
                    pt(2.0, 2.0),
                    pt(8.0, 2.0),
                    pt(8.0, 8.0),
                    pt(2.0, 8.0),
                    pt(2.0, 2.0),
                ],
            ),
        ];

        let mp = assemble(&members);
        assert_eq!(mp.polygons.len(), 1);
        let inner_area = polygon::signed_area(&mp.polygons[0].1[0]);
        assert!(
            inner_area < 0.0,
            "inner ring should have negative signed_area, got {inner_area}",
        );
    }

    // --- Second-pass chain merging recovers from ordering failures ---

    #[test]
    fn test_out_of_order_ways_joined() {
        // A square ring split into 4 ways, given in an order that defeats
        // single-pass greedy joining:
        //   way1: P1->P2  (creates chain 0)
        //   way2: P3->P4  (creates chain 1 - no shared endpoints with chain 0)
        //   way3: P2->P3  (joins chain 0, extending to P1->P2->P3;
        //                   endpoint_map now maps P3->chain0, overwriting chain1's P3)
        //   way4: P4->P1  (joins chain 1, extending to P3->P4->P1;
        //                   endpoint_map now maps P1->chain1, overwriting chain0's P1)
        // After the first pass, chains 0 and 1 are both unclosed but should
        // form a single closed ring together. The second merge pass fixes this.
        let members = vec![
            make_member("outer", vec![pt(0.0, 0.0), pt(1.0, 0.0)]),
            make_member("outer", vec![pt(1.0, 1.0), pt(0.0, 1.0)]),
            make_member("outer", vec![pt(1.0, 0.0), pt(1.0, 1.0)]),
            make_member("outer", vec![pt(0.0, 1.0), pt(0.0, 0.0)]),
        ];

        let mp = assemble(&members);
        assert_eq!(
            mp.polygons.len(),
            1,
            "out-of-order ways should form one polygon"
        );
        assert_eq!(mp.polygons[0].0.len(), 4, "square should have 4 vertices");
    }

    #[test]
    fn test_assembly_is_deterministic_across_member_order() {
        let members_a = vec![
            make_member("outer", vec![pt(0.0, 0.0), pt(2.0, 0.0), pt(2.0, 2.0)]),
            make_member("outer", vec![pt(2.0, 2.0), pt(0.0, 2.0), pt(0.0, 0.0)]),
            make_member("inner", vec![pt(0.5, 0.5), pt(1.5, 0.5), pt(1.5, 1.5)]),
            make_member("inner", vec![pt(1.5, 1.5), pt(0.5, 1.5), pt(0.5, 0.5)]),
        ];
        let members_b = vec![
            make_member("inner", vec![pt(1.5, 1.5), pt(0.5, 1.5), pt(0.5, 0.5)]),
            make_member("outer", vec![pt(2.0, 2.0), pt(0.0, 2.0), pt(0.0, 0.0)]),
            make_member("inner", vec![pt(0.5, 0.5), pt(1.5, 0.5), pt(1.5, 1.5)]),
            make_member("outer", vec![pt(0.0, 0.0), pt(2.0, 0.0), pt(2.0, 2.0)]),
        ];

        let a = assemble(&members_a);
        let b = assemble(&members_b);

        assert_eq!(a.polygons.len(), b.polygons.len());
        for ((a_outer, a_inners), (b_outer, b_inners)) in a.polygons.iter().zip(b.polygons.iter()) {
            assert_eq!(ring_sort_key(a_outer), ring_sort_key(b_outer));
            assert_eq!(ring_keys(a_outer), ring_keys(b_outer));
            assert_eq!(a_inners.len(), b_inners.len());
            for (ai, bi) in a_inners.iter().zip(b_inners.iter()) {
                assert_eq!(ring_sort_key(ai), ring_sort_key(bi));
                assert_eq!(ring_keys(ai), ring_keys(bi));
            }
        }
    }

    #[test]
    fn relation_centroid_aligns_antimeridian_world_copies() {
        let rectangle = |x0, x1| vec![pt(x0, 0.25), pt(x1, 0.25), pt(x1, 0.375), pt(x0, 0.375)];
        let polygons = vec![
            (rectangle(0.875, 1.0), Vec::new()),
            (rectangle(0.0, 0.125), Vec::new()),
        ];
        let point = relation_label_point(&polygons, PolygonPointStrategy::Centroid).unwrap();
        assert!(point.x.rem_euclid(1.0).abs() < 1e-12);
        assert!((point.y - 0.3125).abs() < 1e-12);
    }

    #[test]
    fn relation_surface_skips_degenerate_components() {
        let valid = square(0.25, 0.375);
        let polygons = vec![
            (vec![pt(0.0, 0.0), pt(0.5, 0.5), pt(1.0, 1.0)], Vec::new()),
            (valid.clone(), Vec::new()),
        ];
        for strategy in [
            PolygonPointStrategy::PointOnSurface,
            PolygonPointStrategy::CentroidIfConvex,
        ] {
            let point = relation_label_point(&polygons, strategy).unwrap();
            assert!(polygon::point_in_polygon(&point, &valid));
            assert!(relation_label_point(&polygons[..1], strategy).is_none());
        }
    }

    #[test]
    fn relation_labels_preserve_small_component_areas() {
        let step = 2.0_f64.powi(-30);
        let origin = 0.75;
        let rectangle = |x0, y0, x1, y1| {
            vec![
                pt(origin + x0 * step, origin + y0 * step),
                pt(origin + x1 * step, origin + y0 * step),
                pt(origin + x1 * step, origin + y1 * step),
                pt(origin + x0 * step, origin + y1 * step),
            ]
        };
        let outer = rectangle(0.0, 0.0, 4.0, 4.0);
        let hole = rectangle(1.0, 1.0, 3.0, 3.0);
        let polygons = vec![
            (outer.clone(), vec![hole.clone()]),
            (rectangle(8.0, 0.0, 10.0, 2.0), Vec::new()),
        ];
        let centroid = relation_label_point(&polygons, PolygonPointStrategy::Centroid)
            .expect("small components retain their positive net areas");
        assert!((centroid.x - (origin + 3.75 * step)).abs() < step / 16.0);
        assert!((centroid.y - (origin + 1.75 * step)).abs() < step / 16.0);
        for strategy in [
            PolygonPointStrategy::PointOnSurface,
            PolygonPointStrategy::CentroidIfConvex,
        ] {
            let point = relation_label_point(&polygons, strategy).expect("small surface label");
            assert!(polygon::point_in_polygon(&point, &outer));
            assert!(!polygon::point_in_polygon(&point, &hole));
        }
    }

    #[test]
    fn single_component_relation_addresses_keep_convex_centroids() {
        let outer = vec![pt(0.25, 0.25), pt(0.375, 0.25), pt(0.25, 0.375)];
        let polygons = vec![(outer.clone(), Vec::new())];
        let centroid = geometry::surface::polygon_centroid(&outer, &[]).expect("triangle centroid");
        let point = relation_label_point(&polygons, PolygonPointStrategy::CentroidIfConvex)
            .expect("single-shell address centroid");
        assert!((point.x - centroid.x).abs() < 1e-12);
        assert!((point.y - centroid.y).abs() < 1e-12);
        let surface = relation_label_point(&polygons, PolygonPointStrategy::PointOnSurface)
            .expect("triangle surface point");
        assert!((surface.y - centroid.y).abs() > 0.001);
    }

    // --- Quantize ---

    #[test]
    fn test_quantize_consistency() {
        let p1 = pt(0.123_456_789, 0.987_654_321);
        let p2 = pt(0.123_456_789, 0.987_654_321);
        assert_eq!(quantize(&p1), quantize(&p2));
    }

    #[test]
    fn test_quantize_distinct() {
        let p1 = pt(0.1, 0.2);
        let p2 = pt(0.1, 0.3);
        assert_ne!(quantize(&p1), quantize(&p2));
    }
}
