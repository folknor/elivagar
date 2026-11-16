//! Pairing: deciding which current item corresponds to which baseline item.
//!
//! Anonymous features (and a feature's components) carry no identity across two
//! archives, so before anything can be classified as "moved" the two sides have
//! to be matched up. Getting this wrong inflates the report in the worst way -
//! an unlucky pairing turns one shifted feature into one added plus one missing,
//! which reads as a structural change rather than a displacement.
//!
//! Three tiers, cheapest first:
//!
//! 1. **Exact** - bucket by geometry digest and confirm with a full compare.
//!    Identical geometry pairs up without any distance work.
//! 2. **Residual** - for what is left, a minimum-cost maximum-cardinality
//!    matching over a sparse K-nearest candidate graph, so a cluster of features
//!    that all moved together pairs consistently instead of greedily crossing
//!    over itself. Tiny residual sets skip the machinery and take an exact
//!    greedy pass instead.
//! 3. **Force-zip** - whatever still has no partner is zipped in index order, so
//!    a type or topology change stays one structural movement rather than an
//!    arbitrary added/missing pair.
//!
//! Ported verbatim from elivagar's shed `regress.rs` (commit `0129ef3~1`).

use std::cmp::Ordering;

use rustc_hash::{FxHashMap, FxHashSet};

use super::geometry::{component_distance, feature_distance};
use super::prepared::{PreparedComponent, PreparedFeature, compare_prepared_component_slices};

pub(crate) struct PairResult {
    pub(crate) paired: Vec<(usize, usize)>,
    pub(crate) unpaired_current: Vec<usize>,
    pub(crate) unpaired_baseline: Vec<usize>,
}

// ---------------------------------------------------------------------------
// Feature pairing
// ---------------------------------------------------------------------------

pub(crate) fn pair_detail_features(
    current: &[&PreparedFeature],
    baseline: &[&PreparedFeature],
) -> PairResult {
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; baseline.len()];
    let mut paired = exact_feature_pairs(current, baseline, &mut cur_used, &mut bl_used);
    let residual = remaining_pairs(
        current,
        baseline,
        &mut cur_used,
        &mut bl_used,
        |feature| (feature.geom_type, feature.structure),
        |left, right| left.bbox.lower_bound_sq(right.bbox),
        |left, right| left.bbox.center_distance_sq(right.bbox),
        |left, right| feature_distance(left, right),
    );
    paired.extend(residual);
    finish_pairs(&cur_used, &bl_used, paired)
}

fn exact_feature_pairs(
    current: &[&PreparedFeature],
    baseline: &[&PreparedFeature],
    cur_used: &mut [bool],
    bl_used: &mut [bool],
) -> Vec<(usize, usize)> {
    let mut buckets: FxHashMap<u128, Vec<usize>> = FxHashMap::default();
    for (idx, feature) in baseline.iter().enumerate() {
        buckets
            .entry(feature.geometry_digest)
            .or_default()
            .push(idx);
    }
    let mut paired = Vec::new();
    for (ci, feature) in current.iter().enumerate() {
        let Some(candidates) = buckets.get(&feature.geometry_digest) else {
            continue;
        };
        if let Some(&bi) = candidates
            .iter()
            .find(|&&bi| !bl_used[bi] && detail_geometry_equal(feature, baseline[bi]))
        {
            cur_used[ci] = true;
            bl_used[bi] = true;
            paired.push((ci, bi));
        }
    }
    paired
}

/// The digest is only a bucketing key; this is the confirmation that makes a
/// collision harmless.
fn detail_geometry_equal(left: &PreparedFeature, right: &PreparedFeature) -> bool {
    left.geom_type == right.geom_type
        && compare_prepared_component_slices(&left.components, &right.components) == Ordering::Equal
}

// ---------------------------------------------------------------------------
// Component pairing
// ---------------------------------------------------------------------------

pub(crate) fn pair_detail_components(
    current: &[PreparedComponent],
    baseline: &[PreparedComponent],
) -> PairResult {
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; baseline.len()];
    let mut buckets: FxHashMap<u128, Vec<usize>> = FxHashMap::default();
    for (idx, component) in baseline.iter().enumerate() {
        buckets.entry(component.digest).or_default().push(idx);
    }
    let mut paired = Vec::new();
    for (ci, component) in current.iter().enumerate() {
        if let Some(candidates) = buckets.get(&component.digest)
            && let Some(&bi) = candidates.iter().find(|&&bi| {
                !bl_used[bi]
                    && super::prepared::compare_prepared_components(component, &baseline[bi])
                        == Ordering::Equal
            })
        {
            cur_used[ci] = true;
            bl_used[bi] = true;
            paired.push((ci, bi));
        }
    }
    paired.extend(remaining_pairs(
        current,
        baseline,
        &mut cur_used,
        &mut bl_used,
        |component| component.structure,
        |left, right| left.bbox.lower_bound_sq(right.bbox),
        |left, right| left.bbox.center_distance_sq(right.bbox),
        component_distance,
    ));
    finish_pairs(&cur_used, &bl_used, paired)
}

// ---------------------------------------------------------------------------
// The residual matcher
// ---------------------------------------------------------------------------

/// Match whatever the exact pass left over.
///
/// `key` partitions candidates (only same-key items may pair), `lower` is a
/// cheap admissible lower bound used to order candidates, `proxy` breaks ties
/// among equal lower bounds, and `distance` is the real cost - evaluated once
/// per surviving candidate edge, never over the full cross product.
#[allow(clippy::too_many_arguments)]
pub(crate) fn remaining_pairs<T, K: Eq + std::hash::Hash + Copy>(
    current: &[T],
    baseline: &[T],
    cur_used: &mut [bool],
    bl_used: &mut [bool],
    key: impl Fn(&T) -> K,
    lower: impl Fn(&T, &T) -> u64,
    proxy: impl Fn(&T, &T) -> u64,
    distance: impl Fn(&T, &T) -> i32,
) -> Vec<(usize, usize)> {
    let remaining_current: Vec<usize> = cur_used
        .iter()
        .enumerate()
        .filter_map(|(idx, used)| (!*used).then_some(idx))
        .collect();
    let remaining_baseline: Vec<usize> = bl_used
        .iter()
        .enumerate()
        .filter_map(|(idx, used)| (!*used).then_some(idx))
        .collect();
    // Below this the full cross product is cheaper than building a candidate
    // graph, and exact greedy is what the pre-sparse engine did throughout.
    if remaining_current
        .len()
        .saturating_mul(remaining_baseline.len())
        <= 64
    {
        return exact_greedy_pairs(
            current,
            baseline,
            cur_used,
            bl_used,
            &remaining_current,
            &remaining_baseline,
            distance,
        );
    }

    let candidates = residual_candidates(
        current,
        baseline,
        &remaining_current,
        &remaining_baseline,
        &key,
        &lower,
        &proxy,
    );
    let mut paired =
        sparse_min_cost_pairs(current, baseline, cur_used, bl_used, &candidates, &distance);

    // Same-key completion: the K-nearest candidate graph need not contain a
    // matching that saturates the smaller side of every key group (clusters
    // larger than K can starve each other), and the pre-sparse contract was
    // that same-key pairings are exhausted before any cross-key fallback.
    // Sweep the leftovers with the old proxy-greedy, per key; leftover
    // counts are the starvation excess, so the quadratic edge enumeration
    // stays small.
    let mut completion_edges = Vec::new();
    for &ci in &remaining_current {
        if cur_used[ci] {
            continue;
        }
        for &bi in &remaining_baseline {
            if !bl_used[bi] && key(&current[ci]) == key(&baseline[bi]) {
                completion_edges.push((
                    lower(&current[ci], &baseline[bi]),
                    proxy(&current[ci], &baseline[bi]),
                    ci,
                    bi,
                ));
            }
        }
    }
    completion_edges.sort_unstable();
    for (_, _, ci, bi) in completion_edges {
        if !cur_used[ci] && !bl_used[bi] {
            cur_used[ci] = true;
            bl_used[bi] = true;
            paired.push((ci, bi));
        }
    }

    // A type or topology change is still one structural movement, rather than
    // an arbitrary added/missing pair. This deliberate fallback keeps the old
    // cardinality semantics while avoiding impossible Hausdorff work.
    let left: Vec<usize> = cur_used
        .iter()
        .enumerate()
        .filter_map(|(idx, used)| (!*used).then_some(idx))
        .collect();
    let right: Vec<usize> = bl_used
        .iter()
        .enumerate()
        .filter_map(|(idx, used)| (!*used).then_some(idx))
        .collect();
    for (ci, bi) in left.into_iter().zip(right) {
        cur_used[ci] = true;
        bl_used[bi] = true;
        paired.push((ci, bi));
    }
    paired
}

/// The K-nearest candidate graph, taken from both sides so a baseline item whose
/// best current partner did not reciprocate still gets an edge.
#[allow(clippy::too_many_arguments)]
fn residual_candidates<T, K: Eq + std::hash::Hash + Copy>(
    current: &[T],
    baseline: &[T],
    remaining_current: &[usize],
    remaining_baseline: &[usize],
    key: &impl Fn(&T) -> K,
    lower: &impl Fn(&T, &T) -> u64,
    proxy: &impl Fn(&T, &T) -> u64,
) -> Vec<(usize, usize)> {
    let mut edges = FxHashSet::default();
    for &ci in remaining_current {
        let mut nearest: Vec<_> = remaining_baseline
            .iter()
            .copied()
            .filter(|&bi| key(&current[ci]) == key(&baseline[bi]))
            .map(|bi| {
                (
                    lower(&current[ci], &baseline[bi]),
                    proxy(&current[ci], &baseline[bi]),
                    bi,
                )
            })
            .collect();
        nearest.sort_unstable();
        edges.extend(
            nearest
                .into_iter()
                .take(super::super::constants::RESIDUAL_CANDIDATES)
                .map(|(_, _, bi)| (ci, bi)),
        );
    }
    for &bi in remaining_baseline {
        let mut nearest: Vec<_> = remaining_current
            .iter()
            .copied()
            .filter(|&ci| key(&current[ci]) == key(&baseline[bi]))
            .map(|ci| {
                (
                    lower(&current[ci], &baseline[bi]),
                    proxy(&current[ci], &baseline[bi]),
                    ci,
                )
            })
            .collect();
        nearest.sort_unstable();
        edges.extend(
            nearest
                .into_iter()
                .take(super::super::constants::RESIDUAL_CANDIDATES)
                .map(|(_, _, ci)| (ci, bi)),
        );
    }
    let mut edges: Vec<_> = edges.into_iter().collect();
    edges.sort_unstable();
    edges
}

pub(crate) fn sparse_min_cost_pairs<T>(
    current: &[T],
    baseline: &[T],
    cur_used: &mut [bool],
    bl_used: &mut [bool],
    candidates: &[(usize, usize)],
    distance: &impl Fn(&T, &T) -> i32,
) -> Vec<(usize, usize)> {
    // Successive shortest augmenting paths give a minimum-cost maximum-
    // cardinality matching while evaluating Hausdorff once per candidate
    // edge. Bellman-Ford handles the negative reverse (matched) edges.
    // Relaxation is STRICTLY improving: a predecessor-pointer cycle would
    // require a strict distance decrease around a zero-cost alternating
    // loop, which is impossible, and matchings built by shortest-path
    // augmentation stay extreme, so the residual graph never has a negative
    // cycle and Bellman-Ford converges within one pass per residual vertex.
    // Ties between equal-cost paths resolve to whichever the fixed edge
    // order relaxes first, which keeps the result deterministic.
    let edges: Vec<_> = candidates
        .iter()
        .map(|&(ci, bi)| (ci, bi, i64::from(distance(&current[ci], &baseline[bi]))))
        .collect();
    let edge_cost: FxHashMap<(usize, usize), i64> = edges
        .iter()
        .map(|&(ci, bi, cost)| ((ci, bi), cost))
        .collect();
    let mut residual_baseline: Vec<usize> = candidates.iter().map(|&(_, bi)| bi).collect();
    residual_baseline.sort_unstable();
    residual_baseline.dedup();
    // Candidates are sorted by (ci, bi), so ci values arrive grouped.
    let mut residual_current: Vec<usize> = candidates.iter().map(|&(ci, _)| ci).collect();
    residual_current.dedup();
    let vertex_bound = residual_current.len() + residual_baseline.len();

    let mut cur_match: Vec<Option<usize>> = vec![None; current.len()];
    let mut bl_match: Vec<Option<usize>> = vec![None; baseline.len()];
    let mut cur_cost = vec![0_i64; current.len()];

    loop {
        let mut cur_dist = vec![i64::MAX; current.len()];
        let mut bl_dist = vec![i64::MAX; baseline.len()];
        let mut prev_baseline: Vec<Option<usize>> = vec![None; baseline.len()];
        for &ci in &residual_current {
            if !cur_used[ci] && cur_match[ci].is_none() {
                cur_dist[ci] = 0;
            }
        }

        for _ in 0..=vertex_bound {
            let mut changed = false;
            for &(ci, bi, cost) in &edges {
                if cur_dist[ci] == i64::MAX || cur_match[ci] == Some(bi) {
                    continue;
                }
                let relaxed = cur_dist[ci].saturating_add(cost);
                if relaxed < bl_dist[bi] {
                    bl_dist[bi] = relaxed;
                    prev_baseline[bi] = Some(ci);
                    // The only edge back out of a matched baseline node is its
                    // matched current, so the reverse relaxation rides along
                    // here instead of needing its own scan.
                    if let Some(mi) = bl_match[bi] {
                        let back = relaxed.saturating_sub(cur_cost[mi]);
                        if back < cur_dist[mi] {
                            cur_dist[mi] = back;
                        }
                    }
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let target = (0..baseline.len())
            .filter(|&bi| !bl_used[bi] && bl_match[bi].is_none() && bl_dist[bi] != i64::MAX)
            .min_by_key(|&bi| (bl_dist[bi], bi));
        let Some(mut bi) = target else {
            break;
        };
        // Alternate forward predecessor and matched edge back to a free
        // current. Validate the whole chain before changing either matching
        // table so an inconsistent predecessor leaves the current matching
        // intact and the caller can still report unmatched items.
        let mut augmented = false;
        let mut path = Vec::new();
        for _ in 0..vertex_bound {
            let Some(ci) = prev_baseline[bi] else {
                break;
            };
            let Some(&cost) = edge_cost.get(&(ci, bi)) else {
                break;
            };
            let previous_bi = cur_match[ci];
            if previous_bi.is_some_and(|old_bi| bl_match[old_bi] != Some(ci)) {
                break;
            }
            path.push((ci, bi, cost, previous_bi));
            let Some(old_bi) = previous_bi else {
                augmented = true;
                break;
            };
            bi = old_bi;
        }
        if !augmented {
            break;
        }
        for (ci, bi, cost, previous_bi) in path {
            cur_match[ci] = Some(bi);
            bl_match[bi] = Some(ci);
            cur_cost[ci] = cost;
            if let Some(old_bi) = previous_bi {
                bl_match[old_bi] = None;
            }
        }
    }

    let mut paired = Vec::new();
    for (ci, matched) in cur_match.into_iter().enumerate() {
        if let Some(bi) = matched {
            cur_used[ci] = true;
            bl_used[bi] = true;
            paired.push((ci, bi));
        }
    }
    paired
}

/// Repeatedly take the globally cheapest remaining pair. Quadratic per pick, so
/// only reached for residual sets small enough that the cross product is under
/// the threshold above.
fn exact_greedy_pairs<T>(
    current: &[T],
    baseline: &[T],
    cur_used: &mut [bool],
    bl_used: &mut [bool],
    remaining_current: &[usize],
    remaining_baseline: &[usize],
    distance: impl Fn(&T, &T) -> i32,
) -> Vec<(usize, usize)> {
    let mut paired = Vec::new();
    loop {
        let mut best: Option<(usize, usize, i32)> = None;
        for &ci in remaining_current {
            if cur_used[ci] {
                continue;
            }
            for &bi in remaining_baseline {
                if bl_used[bi] {
                    continue;
                }
                let candidate = distance(&current[ci], &baseline[bi]);
                if best.is_none_or(|(_, _, distance)| candidate < distance) {
                    best = Some((ci, bi, candidate));
                }
            }
        }
        let Some((ci, bi, _)) = best else {
            break;
        };
        cur_used[ci] = true;
        bl_used[bi] = true;
        paired.push((ci, bi));
    }
    paired
}

pub(crate) fn finish_pairs(
    cur_used: &[bool],
    bl_used: &[bool],
    paired: Vec<(usize, usize)>,
) -> PairResult {
    PairResult {
        paired,
        unpaired_current: cur_used
            .iter()
            .enumerate()
            .filter_map(|(idx, used)| (!*used).then_some(idx))
            .collect(),
        unpaired_baseline: bl_used
            .iter()
            .enumerate()
            .filter_map(|(idx, used)| (!*used).then_some(idx))
            .collect(),
    }
}
