# Tile Seam Fix: Buffer Was 16x Too Small

**Date:** 2026-03-06
**Files changed:** `src/geometry.rs`, `src/pipeline.rs`

## Background: The 2026-03-05 Debug Session

The problem was reported as clear square tile-boundary cutoffs where geometry
stops abruptly, visible at every zoom level and across multiple layers. Click
reports in the debug viewer showed tiles returning `layers=[none]` immediately
adjacent to tiles with `layers=[land]`, e.g. z10/537/320 (land) vs
z10/537/321 (none).

A full-day debug session on 2026-03-05 produced:

- **4 debug examples** (`audit_single_tile_seams.rs`, `audit_tile_seams.rs`,
  `probe_shared_edges.rs`, `scan_layer_boundary_consistency.rs`)
- **A comparison viewer** (`notes/debug/shared_edge_viewer.html`) with
  MapLibre + PMTiles for side-by-side tile inspection
- **A range-capable HTTP server** (`notes/debug/range_http_server.py`)
  after `python3 -m http.server` failed on PMTiles range requests
- **~12 iterative PMTiles outputs** (`notes/debug/denmark-seamfix-*.pmtiles`)

Code changes attempted in that session (all later reverted):

1. **Closed-ring normalization** in `clip_polygon_into` - strip duplicate
   closing vertex before Sutherland-Hodgman (SH expects open rings)
2. **`snap_points_to_clip_rect`** - snap points within epsilon of clip edges
   to exact edge values to prevent floating-point boundary cracks
3. **Disabled row pre-clipping** (`multi_row = false`) - hypothesis that
   two-stage clipping (row then tile) produced asymmetric boundary fragments
4. **Removed `INTERIOR_TILE_RING`** fast path and `tile_is_interior` calls
5. **Removed `is_valid_simple_ring_points` / `is_valid_simple_tile_ring`**
   filters at z<14 - hypothesis that these dropped geometry that should emit
6. **Ocean bounds guards** - filter out-of-range tile IDs from DDA edge
   rasterization and scanline fill

None of these produced any visible improvement toward fixing the seam problem.
Some produced visual changes (different artifacts) but none moved the needle
on the actual tile-boundary cutoffs. The session concluded with a
recommendation to build a "feature-level seam tracer" as the next step.

## Starting Point for This Session (2026-03-06)

All failed changes from the previous session were reverted to restore a clean
baseline. The `wire_format.rs` change (adding `"natural"` as tag key #51 for
POI peak/pass) was kept as it was unrelated. The unstaged test additions from
the previous session (`geometry_tests.rs`, `pipeline_tests.rs`) were left in
the working tree.

## Dead Ends Investigated This Session

### 1. Assemble phase review

Read the full assemble phase in `pipeline.rs` - the reader thread that groups
`SortRecord`s by `tile_id`, the encoder thread that builds MVT tiles via
`LayerBuilder`, and the writer thread that feeds compressed tiles to
`PmtilesWriter`. No bugs found: tile grouping correctly transitions on
`tile_id` change, layer indices are extracted from sort keys correctly, and
ocean-only tile skipping is properly gated by `has_non_ocean`.

### 2. Sort and merge review

Read `sort.rs` end to end - `SortWriter` (chunk accumulation + flush),
`SortReader` (k-way merge via binary heap), sort key packing/unpacking. The
merge logic is correct: heap ordering is sound, each chunk is read
sequentially, and the minimum-key record is always popped. A theoretical
chunk-naming issue exists if chunk count exceeds 9999 (4-digit zero-padded
filenames), but Denmark produces ~68 chunks so this is not relevant here.

### 3. Hilbert tile ID addressing

Verified `xy_to_tile_id` and `tile_id_to_zxy` are proper inverses. The
Hilbert curve implementation (`hilbert_xy2d` / `hilbert_d2xy` / `hilbert_rot`)
matches the standard algorithm. If addressing were wrong, the map would be a
scrambled mess rather than showing coherent geometry with edge artifacts.

### 4. MVT encoding review

Confirmed `encode_polygon` correctly handles closed rings (strips closing
vertex, emits MoveTo + LineTo + ClosePath). Delta encoding tracks cursor
across rings. `encode_tile_into` sets extent=4096. The `append_geometry`
function used by merge correctly re-encodes deltas relative to the running
destination cursor, including ClosePath resetting to the last MoveTo.

### 5. Mercator projection and tile coordinate conventions

Verified `project()` and `project_e7()` use y=0 at north, y=1 at south
(standard XYZ convention). `tile_range_in_bbox` floors coordinates correctly.
`ClipRect::for_tile` constructs the clip rect consistently. The viewer's
`lngLatToTile` uses the same formula. No Y-flip or rotation issue.

### 6. Feature merge analysis

`brokkr compare-tiles` between elivagar and Planetiler revealed elivagar
produces ~140x fewer features per tile but nearly identical geometry command
counts. This is because `merge_same_attr_geometries` combines all features
with identical (geom_type, tags) within a tile. Investigated whether this
could cause even-odd fill rule artifacts in MapLibre - concluded it cannot,
because merged rings are non-overlapping (each source polygon is clipped to
the same tile rect, and buffer-zone overlap between different source polygons
would be at most 0.5 pixels with the old buffer).

### 7. Self-comparison (parity vs latest)

`brokkr compare-tiles` between `denmark-latest.pmtiles` and
`denmark-parity-standard.pmtiles` showed the two files are virtually
identical - same feature counts, same command counts across all layers and
zoom levels. The only difference is that `latest` includes the ocean layer
and `parity` was generated with `--no-ocean`. This confirmed the bug was
systemic (present in all outputs) rather than a regression between versions.

### 8. Viewer and HTTP server review

Read `shared_edge_viewer.html` and `range_http_server.py`. The viewer uses
standard MapLibre + pmtiles.js, queries `queryRenderedFeatures` on click,
renders polygon layers in fill or line mode. The range server correctly
implements HTTP byte-range requests. No bugs found in either.

## The Bug

`BUFFER_FRACTION` in `geometry.rs` was defined as:

```rust
pub(crate) const BUFFER_FRACTION: f64 = 8.0 / EXTENT; // 8.0 / 4096.0
```

The comment said "8 pixels" but the math divides by `EXTENT` (4096, the MVT
coordinate extent) instead of by the rendered tile size (256 pixels). Since
one rendered pixel = 16 extent units (`4096 / 256`), the actual buffer was:

    8 / 4096 * 4096 = 8 extent units = 0.5 rendered pixels

The standard MVT buffer is 8 *rendered pixels* = 128 extent units. The buffer
was 16x too small.

## Why It Causes Visible Tile Seams

Vector tiles clip geometry at the tile boundary plus a buffer zone. Adjacent
tiles overlap in this buffer so that renderers (MapLibre, etc.) produce a
seamless image. With only 0.5 pixels of overlap, any sub-pixel rendering,
anti-aliasing, or floating-point rounding makes the tile boundary visible as
a hard edge where geometry abruptly stops.

This affected all zoom levels, all layers, and all geometry types - polygons,
lines, and points near tile edges were all clipped too tightly.

## Why Previous Fixes Had No Effect

The 2026-03-05 debug session investigated clipping correctness (closed-ring
normalization, snap-to-edge, row pre-clipping asymmetry), ocean bounds, ring
validity filters, and interior-tile fast paths. None of these made a visible
difference because the clipping logic itself was correct - it was just
operating on a clip rectangle that barely extended past the tile edge. Making
the clipping more precise doesn't help when the rectangle is wrong.

## The Fix

```rust
// Before (0.5 rendered pixels):
pub(crate) const BUFFER_FRACTION: f64 = 8.0 / EXTENT;     // 8.0 / 4096.0

// After (8 rendered pixels):
pub(crate) const BUFFER_FRACTION: f64 = 8.0 / 256.0;
```

The `INTERIOR_TILE_RING` constant in `pipeline.rs` (the full-tile rectangle
used for interior tiles) was also updated:

```rust
// Before (8 extent units of buffer):
const INTERIOR_TILE_RING: [(i32, i32); 5] =
    [(-8, -8), (4104, -8), (4104, 4104), (-8, 4104), (-8, -8)];

// After (128 extent units = 8 pixels of buffer):
const INTERIOR_TILE_RING: [(i32, i32); 5] =
    [(-128, -128), (4224, -128), (4224, 4224), (-128, 4224), (-128, -128)];
```

## How It Was Found

Comparing elivagar's output against itself (parity vs latest) showed
identical tile content - confirming the bug was systemic, not a regression.
Comparing against Planetiler showed similar geometry (command counts within a
few percent) but vastly different feature counts due to elivagar's
`merge_same_attr_geometries`. The merge was a red herring; both tools produce
the same shapes.

The breakthrough came from reading the constant definitions together:

```rust
pub const EXTENT: f64 = 4096.0;                          // MVT coordinate extent
const PX: i64 = (EXTENT as i64) / 256;                   // 16 extent units per pixel
pub(crate) const BUFFER_FRACTION: f64 = 8.0 / EXTENT;    // "8 pixels" -- but actually 8 extent units
```

The comment on `BUFFER_FRACTION` says "8 pixels / 4096 extent" as if those
are equivalent, but 8 pixels = 128 extent units, not 8.

## Impact

- Tile output size will increase slightly (more geometry in buffer zones)
- Rendering should be seamless across tile boundaries
- No behavioral change to clipping, simplification, or feature emission logic
- All 399 tests pass; clippy clean
