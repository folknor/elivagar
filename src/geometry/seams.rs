use rustc_hash::FxHashMap;

use super::SIMPLIFY_PIXELS;
use super::EXTENT;

/// A reference to where a shared chain appears within a specific ring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainRef {
    /// Index into the input `rings` slice.
    pub ring_idx: usize,
    /// Start vertex index within the ring (inclusive).
    pub start: usize,
    /// Number of vertices in this chain segment within the ring.
    pub len: usize,
    /// True if this ring traverses the chain in the reverse direction
    /// relative to the canonical vertex order.
    pub reversed: bool,
}

/// A contiguous sequence of edges shared between two or more polygon rings.
#[derive(Clone, Debug)]
pub struct SharedChain {
    /// The canonical vertex sequence (in the direction of the first incident ring).
    pub vertices: Vec<(i32, i32)>,
    /// Which rings contain this chain and where.
    pub incidents: Vec<ChainRef>,
}

/// An undirected edge key: `(min_point, max_point)` for hashing.
type EdgeKey = ((i32, i32), (i32, i32));

fn edge_key(a: (i32, i32), b: (i32, i32)) -> EdgeKey {
    if a <= b { (a, b) } else { (b, a) }
}

/// Per-edge record: which ring and which edge index within that ring.
#[derive(Clone, Copy)]
struct EdgeHit {
    ring_idx: usize,
    edge_idx: usize,
}

/// Build edge map and shared-edge lookup from polygon rings.
///
/// Returns `shared_edges`: maps `(ring_idx, edge_idx)` to the list of
/// `EdgeHit`s from *other* rings that share the same undirected edge.
fn build_shared_edge_map(
    rings: &[Vec<(i32, i32)>],
) -> FxHashMap<(usize, usize), Vec<EdgeHit>> {
    let mut edge_map: FxHashMap<EdgeKey, Vec<EdgeHit>> = FxHashMap::default();

    for (ring_idx, ring) in rings.iter().enumerate() {
        if ring.len() < 2 {
            continue;
        }
        for i in 0..(ring.len() - 1) {
            let (a, b) = (ring[i], ring[i + 1]);
            if a == b {
                continue;
            }
            edge_map
                .entry(edge_key(a, b))
                .or_default()
                .push(EdgeHit { ring_idx, edge_idx: i });
        }
    }

    let mut shared: FxHashMap<(usize, usize), Vec<EdgeHit>> = FxHashMap::default();
    for hits in edge_map.values() {
        if hits.len() < 2 {
            continue;
        }
        let first = hits[0].ring_idx;
        if !hits.iter().any(|h| h.ring_idx != first) {
            continue;
        }
        for hit in hits {
            shared
                .entry((hit.ring_idx, hit.edge_idx))
                .or_default()
                .extend(hits.iter().filter(|h| h.ring_idx != hit.ring_idx).copied());
        }
    }
    shared
}

/// Grow a chain from a seed edge in `ring_a` paired with `partner_edge` in `ring_b`.
///
/// Walks forward in ring A and correspondingly in ring B (forward or backward
/// depending on relative winding), extending as long as consecutive edges
/// in both rings match the same undirected edge.
#[allow(clippy::too_many_arguments)]
fn grow_chain(
    rings: &[Vec<(i32, i32)>],
    shared_edges: &FxHashMap<(usize, usize), Vec<EdgeHit>>,
    visited: &mut FxHashMap<(usize, usize), bool>,
    ring_a_idx: usize,
    seed_edge: usize,
    ring_b_idx: usize,
    partner_edge: usize,
) -> SharedChain {
    let ring_a = &rings[ring_a_idx];
    let ring_b = &rings[ring_b_idx];
    let n_a = ring_a.len() - 1;
    let n_b = ring_b.len() - 1;

    // Determine relative direction: opposite winding means A→B matches B←A.
    let opposite_dir = ring_a[seed_edge] == ring_b[partner_edge + 1]
        && ring_a[seed_edge + 1] == ring_b[partner_edge];

    let mut edges_a: Vec<usize> = vec![seed_edge];
    let mut edges_b: Vec<usize> = vec![partner_edge];

    // Extend forward.
    loop {
        let last_a = *edges_a.last().expect("non-empty");
        let last_b = *edges_b.last().expect("non-empty");
        let next_a = (last_a + 1) % n_a;
        let next_b = if opposite_dir {
            (last_b + n_b - 1) % n_b
        } else {
            (last_b + 1) % n_b
        };
        if next_a == seed_edge {
            break; // full loop
        }
        // Stop if either next edge was already consumed by a prior chain
        // (happens when a shared boundary wraps around a ring's start/end seam
        // and both fragments are grown from separate seeds).
        if visited.contains_key(&(ring_a_idx, next_a))
            || visited.contains_key(&(ring_b_idx, next_b))
        {
            break;
        }
        if !shared_edges.contains_key(&(ring_a_idx, next_a))
            || !shared_edges.contains_key(&(ring_b_idx, next_b))
        {
            break;
        }
        let ea = edge_key(ring_a[next_a], ring_a[next_a + 1]);
        let eb = edge_key(ring_b[next_b], ring_b[(next_b + 1) % ring_b.len()]);
        if ea != eb {
            break;
        }
        edges_a.push(next_a);
        edges_b.push(next_b);
    }

    // Mark visited.
    for &ei in &edges_a {
        visited.insert((ring_a_idx, ei), true);
    }
    for &ei in &edges_b {
        visited.insert((ring_b_idx, ei), true);
    }

    // Build canonical vertex sequence from ring A.
    let first = edges_a[0];
    let mut vertices: Vec<(i32, i32)> = Vec::with_capacity(edges_a.len() + 1);
    vertices.push(ring_a[first]);
    for &ei in &edges_a {
        vertices.push(ring_a[(ei + 1) % ring_a.len()]);
    }

    // Debug: verify vertices match ring A.
    debug_assert!(
        vertices.iter().enumerate().all(|(i, &v)| v == ring_a[(first + i) % ring_a.len()]),
        "chain vertices do not match ring A"
    );

    let b_start = if opposite_dir {
        *edges_b.last().expect("non-empty")
    } else {
        edges_b[0]
    };

    SharedChain {
        vertices,
        incidents: vec![
            ChainRef { ring_idx: ring_a_idx, start: first, len: edges_a.len() + 1, reversed: false },
            ChainRef { ring_idx: ring_b_idx, start: b_start, len: edges_a.len() + 1, reversed: opposite_dir },
        ],
    }
}

/// Detect contiguous shared edge chains across polygon rings.
///
/// Takes decoded polygon rings in tile extent coordinates `(i32, i32)`.
/// Rings are expected to be closed (first == last vertex), but the function
/// tolerates unclosed rings by treating them as open polylines.
///
/// Returns shared chains sorted by (first incident ring_idx, start index)
/// for deterministic output.
pub fn detect_shared_chains(rings: &[Vec<(i32, i32)>]) -> Vec<SharedChain> {
    let shared_edges = build_shared_edge_map(rings);
    if shared_edges.is_empty() {
        return Vec::new();
    }

    let mut visited: FxHashMap<(usize, usize), bool> = FxHashMap::default();
    let mut chains: Vec<SharedChain> = Vec::new();

    // Sorted seeds for determinism.
    let mut seeds: Vec<(usize, usize)> = shared_edges.keys().copied().collect();
    seeds.sort();

    for &(ring_idx, edge_idx) in &seeds {
        if visited.contains_key(&(ring_idx, edge_idx)) {
            continue;
        }
        let Some(partners) = shared_edges.get(&(ring_idx, edge_idx)) else {
            continue;
        };
        if rings[ring_idx].len() < 2 {
            continue;
        }

        let mut partner_rings: Vec<usize> = partners.iter().map(|h| h.ring_idx).collect();
        partner_rings.sort();
        partner_rings.dedup();

        for &partner_ring_idx in &partner_rings {
            let Some(hit) = partners.iter().find(|h| h.ring_idx == partner_ring_idx) else {
                continue;
            };
            if visited.contains_key(&(ring_idx, edge_idx))
                && visited.contains_key(&(partner_ring_idx, hit.edge_idx))
            {
                continue;
            }
            if rings[partner_ring_idx].len() < 2 {
                continue;
            }

            chains.push(grow_chain(
                rings,
                &shared_edges,
                &mut visited,
                ring_idx,
                edge_idx,
                partner_ring_idx,
                hit.edge_idx,
            ));
        }
    }

    merge_seam_chains(&mut chains);

    chains.sort_by(|a, b| {
        let (a0, b0) = (&a.incidents[0], &b.incidents[0]);
        a0.ring_idx.cmp(&b0.ring_idx).then(a0.start.cmp(&b0.start))
    });
    chains
}

/// Merge chain fragments that were split at a ring's start/end seam.
///
/// Two chains can be merged when they share the same ring pair (same two
/// ring indices in their incidents) and one chain's last vertex equals the
/// other chain's first vertex. This happens when a shared boundary crosses
/// the arbitrary start/end point of a closed ring.
fn merge_seam_chains(chains: &mut Vec<SharedChain>) {
    // Build index: (ring_a_idx, ring_b_idx) → list of chain indices.
    // Normalize the pair so ring_a < ring_b for consistent lookup.
    let mut pair_map: FxHashMap<(usize, usize), Vec<usize>> = FxHashMap::default();
    for (ci, chain) in chains.iter().enumerate() {
        if chain.incidents.len() != 2 {
            continue;
        }
        let (a, b) = (chain.incidents[0].ring_idx, chain.incidents[1].ring_idx);
        let key = if a <= b { (a, b) } else { (b, a) };
        pair_map.entry(key).or_default().push(ci);
    }

    let mut merged_into: Vec<Option<usize>> = vec![None; chains.len()];

    for group in pair_map.values() {
        if group.len() < 2 {
            continue;
        }
        // Try to merge pairs within this group.
        // A chain's last vertex == another chain's first vertex means they connect.
        for &ci in group {
            if merged_into[ci].is_some() {
                continue;
            }
            loop {
                let tail = chains[ci].vertices.last().copied();
                let Some(tail_v) = tail else { break };

                // Find another chain in the group whose first vertex matches our tail.
                let mut found = None;
                for &cj in group {
                    if cj == ci || merged_into[cj].is_some() {
                        continue;
                    }
                    if chains[cj].vertices.first().copied() == Some(tail_v) {
                        found = Some(cj);
                        break;
                    }
                }
                let Some(cj) = found else { break };

                // Merge cj into ci: append cj's vertices (skip first, it's the shared point).
                let suffix: Vec<(i32, i32)> = chains[cj].vertices[1..].to_vec();
                chains[ci].vertices.extend(suffix);

                // Update chain refs: total len grows.
                let new_len = chains[ci].vertices.len();
                for inc in &mut chains[ci].incidents {
                    inc.len = new_len;
                }

                merged_into[cj] = Some(ci);
            }
        }
    }

    // Remove merged chains (iterate in reverse to preserve indices).
    let mut to_remove: Vec<usize> = merged_into
        .iter()
        .enumerate()
        .filter_map(|(i, m)| m.map(|_| i))
        .collect();
    to_remove.sort_unstable_by(|a, b| b.cmp(a));
    for idx in to_remove {
        chains.swap_remove(idx);
    }
}

// ---------------------------------------------------------------------------
// Shared chain canonicalization
// ---------------------------------------------------------------------------

/// Result of canonicalizing shared chains across polygon rings.
pub struct CanonicalizationResult {
    /// Number of 2-incident chains that were canonicalized.
    pub reconciled: usize,
    /// Number of chains with >2 incidents that were skipped.
    pub skipped: usize,
}

/// For each shared chain with exactly 2 incidents, copy the first incident's
/// vertex sequence to the second incident's ring segment (overwriting it).
///
/// This ensures both rings have identical vertices along the shared boundary.
/// Chains with >2 incidents are skipped (counted in result).
///
/// The `rings` slice must be the same one passed to `detect_shared_chains`.
/// Rings are modified in place.
pub fn canonicalize_shared_chains(
    rings: &mut [Vec<(i32, i32)>],
    chains: &[SharedChain],
) -> CanonicalizationResult {
    let mut reconciled: usize = 0;
    let mut skipped: usize = 0;

    for chain in chains {
        if chain.incidents.len() != 2 {
            skipped += 1;
            continue;
        }

        let canonical = &chain.vertices;
        let target = &chain.incidents[1];
        let ring = &mut rings[target.ring_idx];
        let n = ring.len().saturating_sub(1); // exclude closing vertex
        if n == 0 || target.len != canonical.len() {
            continue;
        }

        // Write canonical vertices into the target ring's segment.
        // If the target traverses the chain in reverse, reverse the canonical order.
        for (i, &v) in canonical.iter().enumerate() {
            let ring_pos = if target.reversed {
                // Reversed: canonical[0] maps to target.start, walking backward.
                (target.start + target.len - 1 - i) % n
            } else {
                (target.start + i) % n
            };
            ring[ring_pos] = v;
        }

        // Fix closing vertex if modified.
        let first = ring[0];
        let last_idx = ring.len() - 1;
        ring[last_idx] = first;

        reconciled += 1;
    }

    CanonicalizationResult { reconciled, skipped }
}

/// Build a pinned-vertex mask for a ring based on shared chain membership.
///
/// Returns a `Vec<bool>` where `pinned[i] = true` means vertex `i` is part of
/// a shared chain and must not be removed by simplification.
pub fn build_pinned_mask(ring_len: usize, ring_idx: usize, chains: &[SharedChain]) -> Vec<bool> {
    let mut pinned = vec![false; ring_len];
    let n = ring_len.saturating_sub(1); // exclude closing vertex
    if n == 0 {
        return pinned;
    }
    for chain in chains {
        for inc in &chain.incidents {
            if inc.ring_idx != ring_idx {
                continue;
            }
            for j in 0..inc.len {
                let pos = (inc.start + j) % n;
                pinned[pos] = true;
            }
            // Also pin the closing vertex if vertex 0 is pinned.
            if pinned[0] {
                pinned[ring_len - 1] = true;
            }
        }
    }
    pinned
}

/// Simplify a ring in tile coordinates, preserving pinned vertices.
///
/// Runs Douglas-Peucker on non-pinned segments. Pinned vertices (from shared
/// chains) survive unconditionally. Tolerance is in tile extent units
/// (16.0 = 1 pixel at extent 4096 / 256 px tiles).
///
/// The ring is modified in place. Returns the new ring.
pub fn simplify_ring_tile_coords(ring: &[(i32, i32)], pinned: &[bool], tolerance: f64) -> Vec<(i32, i32)> {
    if ring.len() < 4 {
        return ring.to_vec();
    }

    // Convert to f64 points for DP.
    let tol_sq = tolerance * tolerance;
    let n = ring.len() - 1; // exclude closing vertex

    // Mark vertices to keep.
    let mut keep = vec![false; n];
    keep[0] = true; // always keep first

    // For each non-pinned segment between pinned boundaries, run DP.
    // First, find segment boundaries (indices where pinned[i] is true).
    let mut boundaries: Vec<usize> = Vec::new();
    for i in 0..n {
        if pinned[i] {
            keep[i] = true;
            boundaries.push(i);
        }
    }

    if boundaries.is_empty() {
        // No pinned vertices - simplify the whole ring.
        dp_tile_coords(ring, 0, n - 1, tol_sq, &mut keep);
    } else {
        // Simplify segments between consecutive pinned boundaries.
        for w in boundaries.windows(2) {
            let (start, end) = (w[0], w[1]);
            if end - start > 1 {
                dp_tile_coords(ring, start, end, tol_sq, &mut keep);
            }
        }
        // Wrap-around: segment from last boundary to first boundary (through ring end).
        let last = *boundaries.last().expect("non-empty");
        let first = boundaries[0];
        if last != first {
            // Segment goes last → n-1 → 0 → first. Only simplify if there
            // are intermediate vertices.
            let gap = (first + n - last) % n;
            if gap > 1 {
                // Linearize the wrap-around segment for DP.
                let mut seg: Vec<(i32, i32)> = Vec::with_capacity(gap + 1);
                for j in 0..=gap {
                    seg.push(ring[(last + j) % n]);
                }
                let mut seg_keep = vec![false; seg.len()];
                seg_keep[0] = true;
                seg_keep[seg.len() - 1] = true;
                dp_tile_coords(&seg, 0, seg.len() - 1, tol_sq, &mut seg_keep);
                // Map back to ring indices.
                for (j, &k) in seg_keep.iter().enumerate() {
                    if k {
                        keep[(last + j) % n] = true;
                    }
                }
            }
        }
    }

    // Build output.
    let mut out: Vec<(i32, i32)> = Vec::new();
    for i in 0..n {
        if keep[i] {
            out.push(ring[i]);
        }
    }
    // Close the ring.
    if let Some(&first) = out.first() {
        out.push(first);
    }
    // Postcondition: a valid closed polygon ring needs at least 4 vertices
    // (3 distinct + closing). If simplification collapsed the ring, fall back
    // to the original.
    if out.len() < 4 {
        return ring.to_vec();
    }
    out
}

/// Douglas-Peucker on tile coordinate points (i32, i32).
/// Marks vertices to keep between `start` and `end` (inclusive, both kept).
fn dp_tile_coords(ring: &[(i32, i32)], start: usize, end: usize, tol_sq: f64, keep: &mut [bool]) {
    keep[start] = true;
    keep[end] = true;
    if end <= start + 1 {
        return;
    }

    let (ax, ay) = (f64::from(ring[start].0), f64::from(ring[start].1));
    let (bx, by) = (f64::from(ring[end].0), f64::from(ring[end].1));
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;

    let mut max_dist_sq: f64 = 0.0;
    let mut max_idx = start;

    for (i, &(ix, iy)) in ring.iter().enumerate().take(end).skip(start + 1) {
        let (px, py) = (f64::from(ix), f64::from(iy));
        let dist_sq = if len_sq < f64::EPSILON {
            (px - ax) * (px - ax) + (py - ay) * (py - ay)
        } else {
            let t = ((px - ax) * dx + (py - ay) * dy) / len_sq;
            let t = t.clamp(0.0, 1.0);
            let proj_x = ax + t * dx;
            let proj_y = ay + t * dy;
            (px - proj_x) * (px - proj_x) + (py - proj_y) * (py - proj_y)
        };

        if dist_sq > max_dist_sq {
            max_dist_sq = dist_sq;
            max_idx = i;
        }
    }

    if max_dist_sq > tol_sq {
        keep[max_idx] = true;
        dp_tile_coords(ring, start, max_idx, tol_sq, keep);
        dp_tile_coords(ring, max_idx, end, tol_sq, keep);
    }
}

/// Tile-coordinate DP tolerance: 1 pixel = EXTENT / 256 = 16 extent units.
pub const TILE_SIMPLIFY_TOLERANCE: f64 = SIMPLIFY_PIXELS * (EXTENT / 256.0);

