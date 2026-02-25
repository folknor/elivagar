// Multipolygon assembly from OSM relation member ways.
//
// Takes a set of member ways (with roles "outer", "inner", or "") and joins
// them end-to-end into closed rings, then pairs inner rings with their
// containing outer ring to produce complete multipolygon geometry.
//
// All coordinates are in Mercator [0,1] space.

use std::collections::HashMap;

use crate::geometry::{self, signed_area, Point};

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

/// A fully assembled multipolygon.
pub struct MultiPolygon {
    /// Each entry is (outer_ring, inner_rings).
    /// Rings do NOT have duplicated closing vertices.
    pub polygons: Vec<(Vec<Point>, Vec<Vec<Point>>)>,
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

/// Assemble a multipolygon from member ways.
///
/// Ways are joined end-to-end by matching endpoints, then classified into
/// outer/inner rings and paired together.
#[hotpath::measure]
pub fn assemble(members: &[MemberWay]) -> MultiPolygon {
    let (outer_ways, inner_ways, unclassified_ways) = separate_by_role(members);

    let (mut outer_rings, _) = join_ways(&outer_ways);
    let (mut inner_rings, _) = join_ways(&inner_ways);

    classify_unclassified(&unclassified_ways, &mut outer_rings, &mut inner_rings);

    ensure_outer_orientation(&mut outer_rings);
    ensure_inner_orientation(&mut inner_rings);

    let polygons = pair_rings(outer_rings, inner_rings);
    MultiPolygon { polygons }
}

/// Separate member ways into outer, inner, and unclassified groups.
///
/// Skips ways with fewer than 2 coordinates.
type RingGroups<'a> = (Vec<&'a [Point]>, Vec<&'a [Point]>, Vec<&'a [Point]>);

fn separate_by_role(members: &[MemberWay]) -> RingGroups<'_> {
    let mut outers = Vec::new();
    let mut inners = Vec::new();
    let mut unclassified = Vec::new();

    for m in members {
        if m.coords.len() < 2 {
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

/// Classify unclassified rings by signed area and add them to the
/// appropriate output list.
fn classify_unclassified(
    unclassified_ways: &[&[Point]],
    outer_rings: &mut Vec<Vec<Point>>,
    inner_rings: &mut Vec<Vec<Point>>,
) {
    let (unc_rings, _) = join_ways(unclassified_ways);
    for ring in unc_rings {
        if ring.len() < 3 {
            continue;
        }
        let area = signed_area(&ring);
        if area > 0.0 {
            // Positive signed_area = CCW in math = CW on screen -> outer
            outer_rings.push(ring);
        } else {
            inner_rings.push(ring);
        }
    }
}

/// Ensure all outer rings have positive signed_area (CW on screen).
fn ensure_outer_orientation(rings: &mut [Vec<Point>]) {
    for ring in rings.iter_mut() {
        if signed_area(ring) < 0.0 {
            ring.reverse();
        }
    }
}

/// Ensure all inner rings have negative signed_area (CCW on screen).
fn ensure_inner_orientation(rings: &mut [Vec<Point>]) {
    for ring in rings.iter_mut() {
        if signed_area(ring) > 0.0 {
            ring.reverse();
        }
    }
}

/// Pair inner rings with their containing outer ring using point-in-polygon.
fn pair_rings(
    outer_rings: Vec<Vec<Point>>,
    inner_rings: Vec<Vec<Point>>,
) -> Vec<(Vec<Point>, Vec<Vec<Point>>)> {
    if outer_rings.is_empty() {
        return Vec::new();
    }

    let mut polygons: Vec<(Vec<Point>, Vec<Vec<Point>>)> = outer_rings
        .into_iter()
        .map(|r| (r, Vec::new()))
        .collect();

    for inner in inner_rings {
        if inner.is_empty() {
            continue;
        }
        let test_pt = &inner[0];
        let mut target_idx: Option<usize> = None;
        for (i, poly) in polygons.iter().enumerate() {
            if geometry::point_in_polygon(test_pt, &poly.0) {
                target_idx = Some(i);
                break;
            }
        }
        let idx = target_idx.unwrap_or(0);
        polygons[idx].1.push(inner);
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
/// Returns `(closed_rings, unclosed_chains)`.
fn join_ways(ways: &[&[Point]]) -> (Vec<Vec<Point>>, Vec<Vec<Point>>) {
    let mut chains: Vec<Vec<Point>> = Vec::new();
    // Maps an endpoint (quantized) to the index in `chains` that has that endpoint.
    let mut endpoint_map: HashMap<(i64, i64), usize> = HashMap::new();
    let mut closed: Vec<Vec<Point>> = Vec::new();

    for way in ways {
        if way.len() < 2 {
            continue;
        }
        // Check if this single way is already a closed ring on its own.
        if quantize(&way[0]) == quantize(&way[way.len() - 1]) {
            let mut ring = way.to_vec();
            ring.pop(); // Remove duplicated closing vertex.
            if ring.len() >= 3 {
                closed.push(ring);
            }
            continue;
        }
        append_way_to_chains(way, &mut chains, &mut endpoint_map, &mut closed);
    }

    // Collect remaining unclosed chains (skip degenerate ones).
    let unclosed: Vec<Vec<Point>> = chains
        .into_iter()
        .filter(|c| c.len() >= 2)
        .collect();

    (closed, unclosed)
}

/// Try to attach a single way to an existing chain, or start a new chain.
///
/// When a chain becomes closed, it is moved to `closed`.
fn append_way_to_chains(
    way: &[Point],
    chains: &mut Vec<Vec<Point>>,
    endpoint_map: &mut HashMap<(i64, i64), usize>,
    closed: &mut Vec<Vec<Point>>,
) {
    let way_front = quantize(&way[0]);
    let way_back = quantize(&way[way.len() - 1]);

    // Try to find a chain endpoint matching our front.
    if let Some(&idx) = endpoint_map.get(&way_front)
        && idx < chains.len() && !chains[idx].is_empty()
    {
        attach_way(idx, way, chains, endpoint_map, closed);
        return;
    }

    // Try to find a chain endpoint matching our back.
    if let Some(&idx) = endpoint_map.get(&way_back)
        && idx < chains.len() && !chains[idx].is_empty()
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
    endpoint_map: &mut HashMap<(i64, i64), usize>,
    closed: &mut Vec<Vec<Point>>,
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
        let mut new_chain: Vec<Point> = way.iter().copied().rev().collect();
        new_chain.extend_from_slice(&chains[idx][1..]);
        chains[idx] = new_chain;
    } else if chain_back == way_back {
        // Append reversed way to end of chain.
        endpoint_map.remove(&chain_back);
        let reversed: Vec<Point> = way.iter().copied().rev().collect();
        chains[idx].extend_from_slice(&reversed[1..]);
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
    endpoint_map: &mut HashMap<(i64, i64), usize>,
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
    endpoint_map: &mut HashMap<(i64, i64), usize>,
    closed: &mut Vec<Vec<Point>>,
) {
    let new_front = quantize(&chains[idx][0]);
    let new_back = quantize(&chains[idx][chains[idx].len() - 1]);

    if new_front == new_back {
        // Chain is closed -- extract it as a ring.
        endpoint_map.remove(&new_front);
        endpoint_map.remove(&new_back);
        let mut ring = std::mem::take(&mut chains[idx]);
        ring.pop(); // Remove duplicated closing vertex.
        if ring.len() >= 3 {
            closed.push(ring);
        }
    } else {
        endpoint_map.insert(new_front, idx);
        endpoint_map.insert(new_back, idx);
    }
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

    // --- point_in_polygon ---

    #[test]
    fn test_point_in_polygon_inside() {
        let ring = vec![pt(0.0, 0.0), pt(1.0, 0.0), pt(1.0, 1.0), pt(0.0, 1.0)];
        assert!(geometry::point_in_polygon(&pt(0.5, 0.5), &ring));
    }

    #[test]
    fn test_point_in_polygon_outside() {
        let ring = vec![pt(0.0, 0.0), pt(1.0, 0.0), pt(1.0, 1.0), pt(0.0, 1.0)];
        assert!(!geometry::point_in_polygon(&pt(2.0, 2.0), &ring));
    }

    #[test]
    fn test_point_in_polygon_degenerate() {
        let ring = vec![pt(0.0, 0.0), pt(1.0, 0.0)];
        assert!(!geometry::point_in_polygon(&pt(0.5, 0.0), &ring));
    }

    #[test]
    fn test_point_in_polygon_triangle() {
        let ring = vec![pt(0.0, 0.0), pt(2.0, 0.0), pt(1.0, 2.0)];
        assert!(geometry::point_in_polygon(&pt(1.0, 0.5), &ring));
        assert!(!geometry::point_in_polygon(&pt(0.0, 2.0), &ring));
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
            make_member("outer", vec![
                pt(0.0, 0.0),
                pt(10.0, 0.0),
                pt(10.0, 10.0),
                pt(0.0, 10.0),
                pt(0.0, 0.0),
            ]),
            // Inner: small square inside the outer
            make_member("inner", vec![
                pt(2.0, 2.0),
                pt(8.0, 2.0),
                pt(8.0, 8.0),
                pt(2.0, 8.0),
                pt(2.0, 2.0),
            ]),
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
            make_member("outer", vec![
                pt(0.0, 0.0),
                pt(1.0, 0.0),
                pt(1.0, 1.0),
                pt(0.0, 1.0),
                pt(0.0, 0.0),
            ]),
            make_member("outer", vec![
                pt(5.0, 5.0),
                pt(6.0, 5.0),
                pt(6.0, 6.0),
                pt(5.0, 6.0),
                pt(5.0, 5.0),
            ]),
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

    // --- Unclassified roles resolved by area ---

    #[test]
    fn test_unclassified_resolved_by_area() {
        // Large CCW ring (positive signed_area -> outer)
        // In standard math: (0,0)->(10,0)->(10,10)->(0,10) is CCW -> positive area
        let members = vec![
            make_member("", vec![
                pt(0.0, 0.0),
                pt(10.0, 0.0),
                pt(10.0, 10.0),
                pt(0.0, 10.0),
                pt(0.0, 0.0),
            ]),
            // Small CW ring (negative signed_area -> inner)
            // (3,3)->(3,7)->(7,7)->(7,3) is CW -> negative area
            make_member("", vec![
                pt(3.0, 3.0),
                pt(3.0, 7.0),
                pt(7.0, 7.0),
                pt(7.0, 3.0),
                pt(3.0, 3.0),
            ]),
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
        let members = vec![
            make_member("outer", vec![
                pt(0.0, 1.0),
                pt(1.0, 1.0),
                pt(1.0, 0.0),
                pt(0.0, 0.0),
                pt(0.0, 1.0),
            ]),
        ];

        let mp = assemble(&members);
        assert_eq!(mp.polygons.len(), 1);
        let area = signed_area(&mp.polygons[0].0);
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
            make_member("outer", vec![
                pt(0.0, 0.0),
                pt(10.0, 0.0),
                pt(10.0, 10.0),
                pt(0.0, 10.0),
                pt(0.0, 0.0),
            ]),
            make_member("inner", vec![
                pt(2.0, 2.0),
                pt(8.0, 2.0),
                pt(8.0, 8.0),
                pt(2.0, 8.0),
                pt(2.0, 2.0),
            ]),
        ];

        let mp = assemble(&members);
        assert_eq!(mp.polygons.len(), 1);
        let inner_area = signed_area(&mp.polygons[0].1[0]);
        assert!(
            inner_area < 0.0,
            "inner ring should have negative signed_area, got {inner_area}",
        );
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
