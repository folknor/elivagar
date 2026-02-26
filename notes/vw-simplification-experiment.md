# Visvalingam-Whyatt Simplification Experiment (2026-02-26)

## Hypothesis

`for_each_zoom_simplified` is the #1 CPU consumer: 64.6s total CPU, 6.6M calls, 27% of
wall time. It runs cascading Douglas-Peucker at each zoom (z14→z0), which is O(n²) per
zoom level. Visvalingam-Whyatt computes vertex importance once in O(n log n), then each
zoom is just O(n) threshold filtering. Expected 2-4x CPU reduction.

## Implementation

Added to `src/geometry.rs`:
- `OrdF64` — newtype for f64 with Eq/Ord via total_cmp (needed for BinaryHeap)
- `triangle_area(a, b, c)` — cross-product effective area
- `vw_compute_importance(points)` — core VW: BinaryHeap min-heap, doubly-linked list,
  lazy deletion of stale entries, monotonicity enforcement. Endpoints get f64::INFINITY.
- `vw_threshold(zoom)` — maps zoom to area threshold: `simplify_tolerance(z)² * 0.5`
- `vw_filter_by_threshold(points, importance, threshold, output)` — O(n) filter

Replaced bodies of `for_each_zoom_simplified` and `for_each_zoom_simplified_multi`.
Same function signatures — no caller changes needed.

## Result 1: VW-only (all geometries)

| Metric | Baseline (DP) | VW-only | Delta |
|--------|---------------|---------|-------|
| Total | 24.3s | 24.9s | **+0.6s (slower)** |
| PBF | 16.5s | 16.8s | +0.3s |
| Assemble | 2.4s | — | — |
| Output | 286 MB | 293 MB | +7 MB |
| Features | 15.98M | — | — |

**VW was slower.** Two reasons:

### 1. Allocation overhead for small geometries

The average geometry has ~10 vertices. VW allocates per call:
- `importance: Vec<f64>` (n elements)
- `alive: Vec<bool>` (n elements)
- `prev_idx: Vec<usize>` (n elements)
- `next_idx: Vec<usize>` (n elements)
- `BinaryHeap<Reverse<(OrdF64, usize)>>` (n-2 initial entries)

That's 5 allocations + heap operations for a 10-vertex geometry where DP does 1
recursive scan with no allocations (reuses caller's buffers). The per-call overhead
exceeded the algorithmic savings.

### 2. VW produces more vertices at low zooms

DP cascade feeds each zoom's simplified output into the next: z14→z13→...→z0. This
"double-simplifies" — z13 is simplified from z14's already-simplified output. The result
is progressively coarser geometry at lower zooms.

VW filters from the **original** geometry at each zoom. z13 gets all points with
importance > threshold(13), regardless of what z14 looked like. This produces more
detailed (higher quality) low-zoom tiles, but more data: +7 MB output, +112K features.

## Result 2: Hybrid DP/VW (threshold at 24 vertices)

Tried dispatching small geometries (≤24 vertices) to DP cascade, large geometries (>24)
to VW. Rationale: VW's O(n log n) should win for large n, DP's zero-alloc approach wins
for small n.

| Metric | Baseline (DP) | Hybrid | Delta |
|--------|---------------|--------|-------|
| Total | 24.3s | 24.5s | +0.2s (noise) |
| PBF | 16.5s | 16.5s | same |
| Assemble | 2.4s | 2.7s | +0.3s |
| Output | 286 MB | 289 MB | +3 MB |
| Features | 15.98M | 16.09M | +112K |

**Still no improvement.** The ~5% of geometries above 24 vertices that use VW don't save
enough CPU to offset the extra output they produce (VW non-cascading → more vertices at
low zooms → more gzip work in assemble).

## Why the profile was misleading

The hotpath profile showed 64.6s total CPU for `for_each_zoom_simplified`, suggesting a
big win from a better algorithm. But:

- **6.6M calls, mostly tiny**: P50 = 2.62µs, P95 = 26.21µs. Most calls process ≤10
  vertices where DP is already fast and VW's allocation overhead dominates.
- **CPU time ≠ wall time**: 64.6s CPU across 12+ rayon threads ≈ 5-6s wall time. Even
  a 2x CPU improvement only saves 2-3s wall time.
- **Cascade is a feature, not a bug**: DP cascade produces less data at low zooms, which
  reduces sort + assemble work. VW's "better quality" geometry costs more downstream.

## Conclusion

Douglas-Peucker cascade is well-suited for this workload:
- Zero extra allocations (reuses caller buffers)
- Fast for small geometries (the vast majority)
- Cascading reduces output size at low zooms (less downstream work)
- Existing convergence tracking + subpixel bbox check already skip unnecessary work

VW would only help if the geometry distribution were different — fewer, larger geometries
where O(n log n) vs O(n²) actually matters. For OSM data (many small ways, avg ~10 nodes),
DP wins.

**Status: Reverted. No code changes merged.**
