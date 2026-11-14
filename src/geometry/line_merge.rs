use rustc_hash::{FxHashMap, FxHashSet};

use super::overlay::IntPoint;

/// Maximum vertex count for a merged linestring. Prevents pathological cases
/// from blowing up tile size. When exceeded, the current segment is finished
/// (no mid-segment truncation) and a new chain starts.
const MAX_LINE_VERTICES: usize = 6000;

// Use the same integer boundary that defines the float line clip buffer.
const CLIP_BOUNDARY_LO: i32 = -crate::geometry::tiles::grid::BUFFER_UNITS;
const CLIP_BOUNDARY_HI: i32 =
    crate::geometry::tiles::grid::EXTENT + crate::geometry::tiles::grid::BUFFER_UNITS;

/// True for a point on the buffered tile boundary.
///
/// The line clipper ends a piece exactly where it leaves the buffered tile,
/// so two different lines that leave through the same quantized point share
/// an endpoint there without being connected in the source. Joining them
/// fabricates an out-and-back (norway z13/4319/2421: two distinct boundary
/// ways, reversed and spliced at (1565,4224)). The position is a conservative
/// proxy for "created by clipping": an authored endpoint that lands on the
/// boundary also stops joining, which splits a sub-line but loses no
/// coverage.
fn on_clip_boundary(point: IntPoint) -> bool {
    point.x == CLIP_BOUNDARY_LO
        || point.x == CLIP_BOUNDARY_HI
        || point.y == CLIP_BOUNDARY_LO
        || point.y == CLIP_BOUNDARY_HI
}

/// Which end of a segment participates at an endpoint.
#[derive(Clone, Copy)]
struct SegEnd {
    seg_idx: usize,
    is_back: bool,
}

/// Working buffers and output of [`merge_line_segments`].
///
/// `witness` records, for every output chain in order, the retained segments
/// it concatenates as `(segment index, traversed reversed)`; `chain_ends[k]`
/// is the exclusive end of chain `k` in `witness`. It is what lets the
/// invariant checker see which joins the merger made, rather than inferring
/// them from output shape, which cannot tell a join from a coincidence.
#[derive(Default)]
pub(crate) struct LineMergeBuffers {
    pub(crate) merged: Vec<Vec<IntPoint>>,
    pub(crate) witness: Vec<(usize, bool)>,
    pub(crate) chain_ends: Vec<usize>,
    visited: Vec<bool>,
    starts: Vec<(i32, i32, usize, bool)>,
    chain: Vec<IntPoint>,
}

/// Merge connected line segments through degree-2 nodes.
///
/// `segments` is deduplicated in place and afterwards holds the retained
/// segments the witness indexes into.
///
/// Pass 1: build chains starting from degree != 2 endpoints (dead ends,
/// junctions) and from every endpoint on the clip boundary, whatever its
/// degree - a chain may start or end there but never continue through it.
/// Pass 2: collect remaining unvisited segments as pure cycles.
/// Traversal order is deterministic (sorted start points and candidate indices).
pub(crate) fn merge_line_segments(segments: &mut Vec<Vec<IntPoint>>, buf: &mut LineMergeBuffers) {
    buf.merged.clear();
    buf.witness.clear();
    buf.chain_ends.clear();
    dedup_parallel_segments(segments);
    if segments.len() < 2 {
        for (i, segment) in segments.iter().enumerate() {
            buf.merged.push(segment.clone());
            buf.witness.push((i, false));
            buf.chain_ends.push(buf.witness.len());
        }
        return;
    }

    // Build endpoint graph
    let mut endpoints: FxHashMap<IntPoint, Vec<SegEnd>> = FxHashMap::default();
    for (i, seg) in segments.iter().enumerate() {
        let front = seg[0];
        let back = seg[seg.len() - 1];
        endpoints.entry(front).or_default().push(SegEnd {
            seg_idx: i,
            is_back: false,
        });
        endpoints.entry(back).or_default().push(SegEnd {
            seg_idx: i,
            is_back: true,
        });
    }

    buf.visited.clear();
    buf.visited.resize(segments.len(), false);

    // Pass 1: chains starting from degree != 2 endpoints and from clip
    // boundary endpoints. A boundary endpoint of degree 2 must be a start
    // too: otherwise a chain passing it could only be picked up in pass 2,
    // which would lose the valid interior joins on either side of it.
    // Sort starts for deterministic output.
    buf.starts.clear();
    for (&point, ends) in &endpoints {
        if ends.len() != 2 || on_clip_boundary(point) {
            for &se in ends {
                buf.starts.push((point.x, point.y, se.seg_idx, se.is_back));
            }
        }
    }
    buf.starts.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
            .then(a.3.cmp(&b.3))
    });

    let starts = std::mem::take(&mut buf.starts);
    for &(_, _, seg_idx, is_back) in &starts {
        if buf.visited[seg_idx] {
            continue;
        }
        build_chain(segments, &endpoints, buf, seg_idx, is_back);
    }
    buf.starts = starts;

    // Pass 2: pure cycles (all unvisited segments).
    // Process in segment-index order for determinism.
    for i in 0..segments.len() {
        if buf.visited[i] {
            continue;
        }
        build_chain(segments, &endpoints, buf, i, false);
    }
}

/// Remove coordinate-identical segments before endpoint graph construction.
/// A segment and its reverse describe the same line, so canonicalize on the
/// lexicographically smaller orientation.
fn dedup_parallel_segments(segments: &mut Vec<Vec<IntPoint>>) {
    let mut seen: FxHashSet<Vec<IntPoint>> = FxHashSet::default();
    segments.retain(|segment| {
        let mut reversed = segment.clone();
        reversed.reverse();
        let key = if segment <= &reversed {
            segment.clone()
        } else {
            reversed
        };
        seen.insert(key)
    });
}

/// Build one chain starting from `seg_idx` entered at `entering_back`.
/// Walks through degree-2 nodes, respecting the vertex cap and never
/// continuing through a clip boundary point. Emits the chain into
/// `buf.merged` and its constituents into the witness.
fn build_chain(
    segments: &[Vec<IntPoint>],
    endpoints: &FxHashMap<IntPoint, Vec<SegEnd>>,
    buf: &mut LineMergeBuffers,
    start_seg: usize,
    entering_back: bool,
) {
    let chain = &mut buf.chain;
    let visited = &mut buf.visited;
    chain.clear();
    let chain_start = buf.witness.len();
    let mut current_seg = start_seg;
    let mut entering_back = entering_back;

    loop {
        if visited[current_seg] {
            break;
        }

        let seg = &segments[current_seg];

        // Vertex cap: finish current segment then stop.
        if !chain.is_empty() && chain.len() + seg.len() > MAX_LINE_VERTICES {
            // Don't mark as visited - will be picked up as a new chain start.
            break;
        }

        visited[current_seg] = true;
        buf.witness.push((current_seg, entering_back));

        // Append segment points (possibly reversed).
        if entering_back {
            if chain.is_empty() {
                chain.extend(seg.iter().rev());
            } else {
                chain.extend(seg.iter().rev().skip(1));
            }
        } else if chain.is_empty() {
            chain.extend_from_slice(seg);
        } else {
            chain.extend_from_slice(&seg[1..]);
        }

        // Find exit point.
        let exit_point = if entering_back {
            seg[0]
        } else {
            seg[seg.len() - 1]
        };

        // A clip boundary point ends the chain whatever its degree: a line
        // meeting another there was cut by the clipper, not connected.
        if on_clip_boundary(exit_point) {
            break;
        }

        // Look for next segment at exit point.
        let Some(ends) = endpoints.get(&exit_point) else {
            break;
        };
        if ends.len() != 2 {
            // Junction or dead end - stop chaining.
            break;
        }

        // Find the other SegEnd (not the one we arrived through).
        // Our exit SegEnd: (current_seg, is_back = !entering_back).
        let our_exit_is_back = !entering_back;
        let other = ends
            .iter()
            .find(|e| !(e.seg_idx == current_seg && e.is_back == our_exit_is_back));
        let Some(&next) = other else {
            // Self-loop: both ends of same segment at same point.
            break;
        };

        // If the "other" is still the same segment (both ends at same point,
        // but different is_back), it's a closed self-loop - stop.
        if next.seg_idx == current_seg {
            break;
        }

        // The upfront dedup makes this unreachable for current inputs, but keep
        // the traversal guard so a future multi-pass merger cannot append a
        // duplicate segment as an out-and-back continuation.
        if segments[next.seg_idx] == *seg || segments[next.seg_idx].iter().rev().eq(seg.iter()) {
            break;
        }

        current_seg = next.seg_idx;
        entering_back = next.is_back;
    }

    if chain.len() >= 2 {
        buf.merged.push(std::mem::take(chain));
        buf.chain_ends.push(buf.witness.len());
    } else {
        buf.witness.truncate(chain_start);
    }
}

/// Unordered adjacent-coordinate pairs of `lines`, with multiplicity,
/// zero-length pairs ignored.
#[cfg(any(test, debug_assertions))]
fn edge_multiset(lines: &[Vec<IntPoint>]) -> std::collections::BTreeMap<[IntPoint; 2], usize> {
    let mut edges = std::collections::BTreeMap::new();
    for line in lines {
        for pair in line.windows(2) {
            if pair[0] == pair[1] {
                continue;
            }
            let edge = if pair[0] <= pair[1] {
                [pair[0], pair[1]]
            } else {
                [pair[1], pair[0]]
            };
            *edges.entry(edge).or_insert(0) += 1;
        }
    }
    edges
}

/// Reference whole-sub-line dedup, deliberately independent of
/// `dedup_parallel_segments`: a line and its reverse are one line.
#[cfg(any(test, debug_assertions))]
fn reference_dedup(input: &[Vec<IntPoint>]) -> Vec<Vec<IntPoint>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for line in input {
        let reversed: Vec<IntPoint> = line.iter().rev().copied().collect();
        let canonical = line.clone().min(reversed);
        if seen.insert(canonical) {
            out.push(line.clone());
        }
    }
    out
}

/// A failed witness invariant, independent of its diagnostic wording.
#[cfg(any(test, debug_assertions))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MergeViolation {
    SegmentOutOfRange { index: usize, retained: usize },
    Consumption { consumed: usize, expected: usize },
    ChainCount { chains: usize, outputs: usize },
    Disconnected { chain: usize, junction: IntPoint },
    ClipBoundary { chain: usize, junction: IntPoint },
    Reconstruction { chain: usize },
    Conservation,
}

#[cfg(any(test, debug_assertions))]
impl std::fmt::Display for MergeViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SegmentOutOfRange { index, retained } => {
                write!(f, "witness names segment {index} of {retained}")
            }
            Self::Consumption { consumed, expected } => write!(
                f,
                "chains consume {consumed} segments, independent dedup keeps {expected}"
            ),
            Self::ChainCount { chains, outputs } => {
                write!(f, "{chains} witness chains for {outputs} output lines")
            }
            Self::Disconnected { chain, junction } => write!(
                f,
                "chain {chain} joins disconnected segments at {junction:?}"
            ),
            Self::ClipBoundary { chain, junction } => write!(
                f,
                "chain {chain} continues through clip boundary point {junction:?}"
            ),
            Self::Reconstruction { chain } => {
                write!(f, "chain {chain} does not match its constituents")
            }
            Self::Conservation => f.write_str("output edges differ from the deduplicated input"),
        }
    }
}

#[cfg(any(test, debug_assertions))]
impl std::error::Error for MergeViolation {}

/// Checks what one [`merge_line_segments`] call did, from its witness:
///
/// - consumption: the chains together use every retained segment exactly
///   once, and the retained set equals an independent dedup of the input;
/// - reconstruction: each chain equals its constituents concatenated in the
///   recorded orientation;
/// - forbidden continuation: no join between consecutive constituents lies
///   on the clip boundary;
/// - conservation: the decoded output carries exactly the edges of the
///   deduplicated input.
///
/// Violations are described, not asserted, so tests can prove each check
/// fires on the transformation it guards against.
#[cfg(any(test, debug_assertions))]
pub(crate) fn check_merge(
    input: &[Vec<IntPoint>],
    retained: &[Vec<IntPoint>],
    merged: &[Vec<IntPoint>],
    decoded: &[Vec<IntPoint>],
    witness: &[(usize, bool)],
    chain_ends: &[usize],
) -> Result<(), MergeViolation> {
    let expected = reference_dedup(input);
    let canonical = |line: &[IntPoint]| {
        let reversed: Vec<IntPoint> = line.iter().rev().copied().collect();
        line.to_vec().min(reversed)
    };
    let mut expected_lines: Vec<_> = expected.iter().map(|l| canonical(l)).collect();
    let mut consumed: Vec<_> = witness
        .iter()
        .map(|&(idx, _)| {
            retained
                .get(idx)
                .map(|l| canonical(l))
                .ok_or(MergeViolation::SegmentOutOfRange {
                    index: idx,
                    retained: retained.len(),
                })
        })
        .collect::<Result<_, _>>()?;
    expected_lines.sort();
    consumed.sort();
    if consumed != expected_lines {
        return Err(MergeViolation::Consumption {
            consumed: consumed.len(),
            expected: expected_lines.len(),
        });
    }

    if chain_ends.len() != merged.len() {
        return Err(MergeViolation::ChainCount {
            chains: chain_ends.len(),
            outputs: merged.len(),
        });
    }
    let mut begin = 0;
    for (k, &end) in chain_ends.iter().enumerate() {
        let parts = &witness[begin..end];
        let mut rebuilt: Vec<IntPoint> = Vec::new();
        for (n, &(idx, reversed)) in parts.iter().enumerate() {
            let seg = &retained[idx];
            let oriented: Vec<IntPoint> = if reversed {
                seg.iter().rev().copied().collect()
            } else {
                seg.clone()
            };
            if n > 0 {
                let junction = oriented[0];
                if rebuilt.last() != Some(&junction) {
                    return Err(MergeViolation::Disconnected { chain: k, junction });
                }
                if on_clip_boundary(junction) {
                    return Err(MergeViolation::ClipBoundary { chain: k, junction });
                }
                rebuilt.extend_from_slice(&oriented[1..]);
            } else {
                rebuilt = oriented;
            }
        }
        if rebuilt != merged[k] {
            return Err(MergeViolation::Reconstruction { chain: k });
        }
        begin = end;
    }

    if edge_multiset(decoded) != edge_multiset(&expected) {
        return Err(MergeViolation::Conservation);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) const TEST_CLIP_BOUNDARY: (i32, i32) = (CLIP_BOUNDARY_LO, CLIP_BOUNDARY_HI);

#[cfg(test)]
mod violation_tests {
    use super::*;

    #[test]
    fn checker_reports_each_witness_violation() {
        let a = IntPoint::new(0, 0);
        let b = IntPoint::new(10, 0);
        let c = IntPoint::new(20, 0);
        let input = vec![vec![a, b], vec![b, c]];
        let merged = vec![vec![a, b, c]];
        let witness = [(0, false), (1, false)];
        let check = |output: &[Vec<IntPoint>],
                     decoded: &[Vec<IntPoint>],
                     parts: &[(usize, bool)],
                     ends: &[usize]| {
            check_merge(&input, &input, output, decoded, parts, ends)
        };
        assert!(matches!(
            check(&merged, &merged, &[(2, false)], &[1]),
            Err(MergeViolation::SegmentOutOfRange {
                index: 2,
                retained: 2
            })
        ));
        assert!(matches!(
            check(&merged, &merged, &[(0, false)], &[1]),
            Err(MergeViolation::Consumption {
                consumed: 1,
                expected: 2
            })
        ));
        assert!(matches!(
            check(&merged, &merged, &witness, &[]),
            Err(MergeViolation::ChainCount {
                chains: 0,
                outputs: 1
            })
        ));
        assert!(matches!(
            check(&merged, &merged, &[(0, false), (1, true)], &[2]),
            Err(MergeViolation::Disconnected { chain: 0, .. })
        ));
        assert!(matches!(
            check(&input, &merged, &witness, &[2, 2]),
            Err(MergeViolation::Reconstruction { chain: 0 })
        ));
        assert_eq!(
            check(&merged, &input[..1], &witness, &[2]),
            Err(MergeViolation::Conservation)
        );
    }
}
