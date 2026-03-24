# MVT Rendering Investigation Plan

**Problem**: Elivagar tiles look correct when rendered as SVG and when processed
through JSTS, but produce rendering artifacts in MapLibre GL JS at all zoom
levels. 160+ dev-hours across 4 developers have not identified the root cause.
Suggestions from Tilemaker, Planetiler, and Tippecanoe devs have been
implemented without improvement.

**Key diagnostic fact**: SVG and JSTS consume the same underlying geometry data
and produce correct results. Only the MapLibre GL JS consumer shows artifacts.
This strongly suggests the problem is in the MVT byte encoding — not the
geometry pipeline itself — since MapLibre's built-in protobuf decoder interprets
something differently than SVG/JSTS decoders.

## Results (2026-03-24)

**The encoder is innocent. The geometry data is the problem.**

All encoder-focused approaches have been executed. None changed the artifacts:

| Approach | Result | Commit |
|---|---|---|
| 0: Protobuf field reordering (15,1,5,2,3,4 → 1,2,3,4,5,15) | No visual change | `e1f0862` |
| 0b: Winding order | Already correct (close_and_orient_cw/ccw at all emit sites) | — |
| 1: Encoder swap (mvt crate, Option B — geometry-level) | Much worse (third-party encoder produces worse output from same data) | — |
| 3: Round-trip re-encode (@mapbox/vector-tile decode → vt-pbf re-encode) | Identical artifacts (32,667 tiles, 0 decode errors) | — |
| 5: MVT compliance validation (Rust verify + vtvalidate) | Found 3 malformed tiles from u16 wire format bug (fixed in `197f6b3`), 15 self-intersecting ocean rings. No other spec violations. | `197f6b3` |

**Conclusion**: The problem is upstream of the encoder — in the polygon
coordinates produced by Sutherland-Hodgman clipping. S-H produces
self-intersecting (figure-8) rings when clipping concave coastline polygons.
SVG handles these correctly via `fill-rule="evenodd"` (winding-agnostic).
MapLibre's earcut tessellation cannot handle self-intersecting input and
produces garbage triangles.

**The fix must be in the geometry pipeline**, not the encoder. Options:
1. Replace S-H with a concave-polygon-safe clipper (Clipper2, Weiler-Atherton)
2. Post-clip repair (split_figure8_ring gave partial success at z4-7)
3. Hybrid: keep S-H for speed, add post-clip boolean repair for polygons

Approaches 2 (binary diff) and 4 (visual regression) are no longer needed
for diagnosis but may be useful for validating the geometry fix.

---

## Approach 0: Protobuf Field Ordering (check first — 10 minutes)

**Status**: Not yet checked.

Elivagar's `LayerBuilder::encode` (`src/mvt/mod.rs:297`) writes layer fields in
this order:

```
field 15 (version)  →  field 1 (name)  →  field 5 (extent)  →
field 2 (features)  →  field 3 (keys)  →  field 4 (values)
```

That's **15, 1, 5, 2, 3, 4** — not monotonically increasing.

The protobuf wire format spec technically allows fields in any order. However:

1. The spec *recommends* serializing in field-number order for compatibility.
2. Some decoders (especially hand-optimized C/C++ decoders like the one in
   MapLibre Native, or fast-path JS parsers) may assume monotonic field order
   and behave incorrectly when encountering field 15 before field 1.
3. `@mapbox/vector-tile` (the JS decoder MapLibre GL JS uses) is built on `pbf`
   (npm), which does handle out-of-order fields — but there may be edge cases
   around repeated fields (features are repeated field 2, interleaved between
   keys/values which are fields 3/4).
4. The MVT spec itself lists fields as: name=1, features=2, keys=3, values=4,
   extent=5, version=15. Planetiler, Tilemaker, and tippecanoe all write them
   in ascending field-number order.

**Fix**: Reorder the writes in `LayerBuilder::encode` to:

```
field 1 (name)  →  field 2 (features)  →  field 3 (keys)  →
field 4 (values)  →  field 5 (extent)  →  field 15 (version)
```

This is trivial (move 3 lines), zero-risk, and eliminates a variable. Do this
first regardless of whether it's the root cause — there's no reason to deviate
from what every other tile generator does.

---

## Approach 0b: Winding Order Enforcement (likely already tried)

**Status**: Probably attempted in the 160-hour branch — verify before
re-implementing.

Elivagar's `encode_polygon` (`src/mvt/mod.rs:436`) faithfully encodes whatever
ring coordinates it receives. It does not calculate, verify, or enforce winding
order. The rings arrive from geometry processing (clipping, simplification,
multipolygon assembly) and are encoded as-is.

The MVT v2.1 spec requires (using the surveyor's formula in tile screen
coordinates, where Y+ is down):
- **Exterior rings**: clockwise (positive signed area)
- **Interior rings (holes)**: counterclockwise (negative signed area)

This is the **opposite** of OGC/GeoJSON convention (CCW exterior, CW interior),
because MVT uses screen coordinates with a flipped Y axis.

### Why SVG works but MapLibre doesn't

Elivagar's SVG renderer (`src/svg.rs`) uses `fill-rule="evenodd"`. The evenodd
rule determines inside/outside by counting ray crossings — it is completely
**winding-agnostic**. A polygon with backwards winding looks identical.

MapLibre GL JS's polygon fill shader uses the **nonzero** winding rule (the GPU
default via stencil buffer operations). With nonzero, winding order determines
which side of a ring is "inside." If an exterior ring is wound CCW instead of
CW, the fill appears inverted — or if interior rings match the exterior winding,
holes disappear and overlap regions create artifacts.

JSTS is also typically winding-agnostic for display (it normalizes internally).

This is the single most common cause of "looks fine everywhere except MapLibre."

### Where winding could go wrong

1. **OSM source data**: OSM ways have no guaranteed winding. Multipolygon
   relation rings can be in any order.
2. **`multipolygon.rs` assembly**: Assembles rings from relation members. May
   or may not enforce output winding.
3. **`geometry.rs` clipping (Sutherland-Hodgman)**: Clipping a correctly-wound
   polygon can produce a ring with reversed winding, depending on the
   implementation. This is a known gotcha with S-H.
4. **`geometry.rs` simplification (Douglas-Peucker)**: Simplification preserves
   winding but can create self-intersections that confuse the nonzero fill rule.
5. **Ocean shapefile processing**: The ocean shapefiles have their own winding
   convention — if not normalized to MVT convention, ocean polygons would be
   affected.

### Fix

Add a winding-order enforcement step just before `encode_polygon` — compute the
signed area of each ring and reverse if needed:

```rust
fn signed_area_2x(ring: &[(i32, i32)]) -> i64 {
    // Shoelace formula. Positive = CW in screen coords (Y+ down).
    ring.windows(2)
        .map(|w| (w[1].0 as i64 - w[0].0 as i64) * (w[1].1 as i64 + w[0].1 as i64))
        .sum()
}

// Before encoding:
// if exterior && signed_area_2x(ring) < 0 { ring.reverse(); }
// if interior && signed_area_2x(ring) > 0 { ring.reverse(); }
```

If this has already been tried in the other branch and didn't help, that's a
valuable data point — it means the winding order is already correct (or the
problem is elsewhere). Worth confirming by running Approach 5 (vtvalidate) which
reports winding violations.

---

## Approach 1: Drop-in MVT Encoder Swap

**Goal**: Definitively answer "is our MVT encoder the problem?"

**Concept**: Replace elivagar's hand-rolled MVT protobuf encoder with a
known-good third-party encoder. If artifacts disappear, the encoder is at fault.
If they persist, the encoder is innocent and the problem is upstream (geometry
clipping, simplification, sort records, etc.).

### Candidate: `mvt` crate (DougLau/mvt)

- **crates.io**: `mvt` (v0.10.3, Rust 2024 edition, actively maintained)
- **Spec**: MVT v2.1
- **API**: Builder pattern — create `Tile`, add `Layer`s, build `Feature`s via
  `GeomEncoder` (takes `GeomType` + `.point(x, y)` calls), then
  `tile.to_bytes()` to produce protobuf.
- **Assessment**: Best drop-in candidate. Focused encoding library (not a tile
  server). Operates at a higher abstraction level than our raw command encoder —
  it takes float tile coordinates and produces valid protobuf. Adds some
  overhead vs our encoder, but this is a diagnostic tool, not a production
  replacement.

### Candidate: `geozero` with `with-mvt` feature

- **crates.io**: `geozero` (v0.15.1, georust org, well-maintained)
- **API**: Trait-based streaming via `GeomProcessor`/`FeatureProcessor`. The
  `MvtWriter` implements these traits. Zero-copy philosophy.
- **Assessment**: Very well maintained (Pirmin Kalberer, author of t-rex).
  Powerful but the streaming API is quite different from our encoder's shape.
  Better suited for format-conversion pipelines. Not a natural drop-in.

### Implementation plan

The swap point is `encode_tile_into()` in `src/mvt/mod.rs`. This function takes
`&[&LayerBuilder]` and writes protobuf bytes into a buffer. Each `LayerBuilder`
has features with pre-encoded `geometry: Vec<u32>` (MVT command sequences) and
interned `tags: Vec<(u16, u16)>`.

**Option A — Replace at the protobuf level**: Keep our `LayerBuilder` pipeline
intact. Write a new `encode_tile_into` that reads the same `LayerBuilder` data
but feeds it through the `mvt` crate's encoder instead of our hand-rolled
protobuf writer. This tests whether our protobuf byte generation is wrong.

**Option B — Replace at the geometry level**: Feed raw (x, y) coordinates (pre
MVT-command-encoding) into the `mvt` crate's `GeomEncoder`. This is deeper —
it also tests whether our MVT command encoding (`encode_polygon`,
`encode_linestring`, etc.) is wrong. More work to wire up, but more thorough.

**Recommendation**: Start with Option A. It's less invasive and tests the most
likely failure point (protobuf structure). If Option A doesn't fix it, do
Option B to test command encoding. If neither fixes it, the encoder is innocent.

### What this tells us

| Result | Conclusion |
|---|---|
| Option A fixes it | Bug in protobuf field encoding (tag ordering, length-delimited framing, value encoding) |
| Option B fixes it but A doesn't | Bug in MVT command sequences (MoveTo/LineTo/ClosePath encoding, zigzag, cursor state) |
| Neither fixes it | Encoder is innocent — problem is in geometry data upstream |

---

## Approach 2: Binary Diff Against Reference Tileset

**Goal**: Find exactly what differs between elivagar's tiles and a known-good
tileset, tile by tile, field by field.

**Concept**: Generate tiles for the same region (Denmark) with both elivagar and
Planetiler. Decode both with a reference MVT decoder. Structurally diff every
field. This produces concrete, specific differences — not "something looks
wrong" but "tile z5/16/10: elivagar water_polygons ring 0 has CCW winding,
Planetiler has CW."

### Decoder options

**Node.js: `@mapbox/vector-tile`**
- npm package `@mapbox/vector-tile` (aka `vector-tile-js`)
- Standard reference decoder used by MapLibre GL JS itself
- Latest: v2.0.4
- Usage: `new VectorTile(new Protobuf(buffer))` → iterate layers → features →
  geometry
- Ideal because this is the *exact decoder MapLibre uses*. If it reads our tile
  differently than Planetiler's, that's the discrepancy MapLibre sees.

**Python: `mapbox-vector-tile`**
- PyPI package `mapbox-vector-tile` (maintained by Tilezen)
- Decodes to Python dicts with geometry as coordinate arrays
- Good for scripting batch comparisons

**CLI: `tippecanoe-decode`**
- Part of Felt's tippecanoe. Decodes MVT tiles to GeoJSON.
- Detects and warns about winding order and ring closure issues.
- Quick way to spot-check individual tiles.

### Implementation plan

1. **Generate both tilesets**: Run elivagar and Planetiler on the same Denmark
   PBF, same zoom range.

2. **Build a comparison script** (Node.js or Python) that:
   - Reads both PMTiles archives
   - For each tile coordinate present in both:
     - Decodes using the reference decoder
     - Compares: layer names, feature counts per layer, geometry types
     - For each matching feature: compare coordinate sequences, winding orders,
       ring counts, tag keys/values
   - Produces a summary report categorizing discrepancies

3. **Sampling strategy**: Don't compare every tile — start with a stratified
   sample:
   - All tiles at z0-z4 (small count, high impact)
   - Random sample of ~100 tiles at z5-z10
   - Specific tiles where artifacts have been visually observed
   - Tiles at tile boundaries (seam issues)

### What to diff specifically

- **Layer names and presence**: Does elivagar emit layers Planetiler doesn't, or
  vice versa?
- **Feature count per layer**: More or fewer features?
- **Geometry type**: Does elivagar emit a polygon where Planetiler emits a
  multi-polygon, or vice versa?
- **Winding order**: Calculate signed area of each polygon ring. MVT spec
  requires CW exterior, CCW interior (in tile screen coordinates where Y+
  is down). This is the #1 suspect for "looks fine in SVG, broken in MapLibre"
  because SVG uses `evenodd` fill-rule (winding-agnostic) while MapLibre's
  polygon fill shader uses winding to distinguish exterior from interior rings.
- **Ring closure**: Are polygon rings properly closed (implicit via ClosePath
  command)?
- **Degenerate geometry**: Zero-area polygons, zero-length lines, single-point
  linestrings.
- **Extent consistency**: Both should claim 4096. If one doesn't, coordinates
  are misinterpreted.
- **Coordinate ranges**: Are any coordinates outside [0, 4096]? Overflows?
- **MVT command validity**: Correct MoveTo/LineTo/ClosePath sequences.
- **Tag encoding**: Same keys and values for matching features?

### Priority fields

If you had to check only three things, check:
1. Winding order of polygon rings
2. Geometry command sequences (valid MoveTo → LineTo → ClosePath pattern)
3. Feature count per layer per tile (missing or duplicated features)

---

## Approach 3: Round-Trip Re-Encode Test

**Goal**: Determine if MapLibre's decoder reads our bytes incorrectly, or if the
decoded geometry itself is wrong.

**Concept**: Take elivagar's PMTiles output → decode each tile with
`@mapbox/vector-tile` → re-encode with a reference encoder → serve the
re-encoded tiles to MapLibre.

### Tools

- **Decode**: `@mapbox/vector-tile` (Node.js) — this is what MapLibre uses
  internally, so it reads our bytes the same way MapLibre would.
- **Re-encode**: `vt-pbf` (npm package) — takes decoded VectorTile objects and
  re-serializes to protobuf. Or `geojson-vt` + `vt-pbf` for a GeoJSON
  intermediate.

### Implementation plan

1. Write a Node.js script that:
   - Opens elivagar's PMTiles output
   - For each tile: decompress → decode with `@mapbox/vector-tile` → re-encode
     with `vt-pbf` → recompress → write to a new PMTiles/MBTiles
   - Serve the re-encoded tiles to MapLibre

2. Compare:
   - **Re-encoded tiles render correctly**: Our protobuf bytes are structured in
     a way that `@mapbox/vector-tile` can parse correctly, but MapLibre's
     *internal* decoder (which may have subtly different fast paths) disagrees.
     This would point to an obscure encoding edge case.
   - **Re-encoded tiles still render incorrectly**: The reference decoder itself
     reads garbage from our tiles. This means the protobuf structure is more
     fundamentally wrong — field numbers, wire types, length prefixes, etc.

### Note on MapLibre's decoder

MapLibre GL JS uses `@mapbox/vector-tile` as its MVT decoder (it's a direct
dependency). So if `@mapbox/vector-tile` can decode our tiles correctly and
`vt-pbf` re-encodes them correctly, the re-encoded tiles *should* render
identically. If they do render correctly, the difference must be in the raw
protobuf byte patterns — perhaps field ordering, varint encoding quirks, or
length-delimited framing that confuses a fast-path parser but not the standard
decoder.

Actually — wait. If MapLibre uses `@mapbox/vector-tile` as its decoder, then
this round-trip test may produce identical results by definition. The real value
of this approach is if we use a *different* decoder (e.g., the Python
`mapbox-vector-tile`) to ensure that the decoded geometry is correct independent
of `@mapbox/vector-tile`.

### Revised implementation plan

Two variants:

**Variant A**: Decode with `@mapbox/vector-tile`, inspect the decoded geometry
programmatically (check feature counts, coordinate ranges, winding orders).
Don't re-encode — just verify the decode output is sane.

**Variant B**: Decode with Python `mapbox-vector-tile` (completely independent
decoder), re-encode with `mapbox-vector-tile`'s encoder, write to new archive,
serve to MapLibre. This tests whether a fully independent decode→re-encode
cycle "cleanses" whatever is wrong.

---

## Approach 4: Headless Visual Regression CI

**Goal**: Eliminate manual visual inspection. Automated pass/fail on every
commit, testing hundreds of tiles in seconds.

### Tool options

**Option A: `@maplibre/maplibre-gl-native` (Node.js)**
- Official Node.js bindings for MapLibre Native
- `map.render()` → raw RGBA buffer → convert to PNG with `sharp`
- Requires OpenGL on Linux (Mesa/llvmpipe for headless)
- Most direct approach — no browser overhead

**Option B: Puppeteer + MapLibre GL JS**
- Run MapLibre GL JS in headless Chromium
- Matches browser rendering exactly (this IS the renderer users see)
- MapLibre's own test suite uses this approach
- Caveat: tile rendering can fail in `headless: true` mode; use
  `headless: "new"` in modern Chrome
- More setup, but higher fidelity

**Option C: `pymgl` (Python)**
- Python bindings to maplibre-native
- Takes GL style JSON, renders to PNG buffer
- Needs XVFB on headless Linux
- Good for Python-based test pipelines

### Implementation plan

1. **Generate reference images**: Render N tiles from a known-good Planetiler
   tileset at various zooms. Save as PNGs. These are the "golden" reference
   images.

2. **Render elivagar tiles**: Same tile coordinates, same style, same zoom
   levels. Save as PNGs.

3. **Pixel diff**: Use `pixelmatch` (npm) or `Pillow` (Python) for perceptual
   image comparison. Set a threshold (e.g., <1% pixel difference = pass).

4. **Report**: For each tile, output pass/fail + diff image highlighting
   changed pixels.

5. **CI integration**: Run after every pipeline change. Fail the build if any
   tile exceeds the diff threshold.

### Tile selection

- All tiles at z0-z6 (manageable count, covers both simple and detailed water)
- Sample of ~50 tiles at z7-z14 covering: coastlines, urban areas, large water
  bodies, country boundaries, areas with known artifacts
- Edge tiles (tiles at the boundary of the Denmark bounding box)

### Style

Use a minimal MapLibre style that exercises all 26 Shortbread layers — you want
to test all layers, not just water. The existing Shortbread reference style
works. Alternatively, render each layer group separately (water, land, streets,
etc.) so you can attribute artifacts to specific layers.

### Bootstrap problem

This approach is most valuable *after* you've fixed the artifacts (to prevent
regressions), or when combined with a reference tileset (Planetiler) to detect
differences. It doesn't tell you *what* is wrong — only *where* and *whether*
something is wrong.

---

## Approach 5: MVT Compliance Validation

**Goal**: Run every tile through a strict validator that checks all MVT spec
requirements. Find spec violations that MapLibre cares about but SVG/JSTS
decoders are lenient about.

### Validator options

**`vtvalidate` (Mapbox)**
- npm: `@mapbox/vtvalidate`
- Backed by the C++ `vtzero` library (gold standard for MVT parsing)
- Checks: protobuf structure, geometry command validity
  (MoveTo/LineTo/ClosePath sequences), layer/feature structure, winding order
- Returns empty string for valid tiles, or a descriptive error string
- Does NOT check: self-intersections
- Input: raw (uncompressed) protobuf buffer

**`vtzero` (Mapbox, C++)**
- Header-only C++ library
- Most thorough low-level validator available
- `decode_polygon_geometry()` with handler callback — `ring_end()` tells you
  if ring is outer, inner, or invalid (wrong winding)
- Throws `geometry_exception` for invalid command sequences,
  `format_exception` for invalid protobuf
- Can be used from Rust via FFI if needed, though the npm wrapper is easier

**`mvt-fixtures` (Mapbox)**
- npm: `@mapbox/mvt-fixtures`
- Suite of valid and invalid MVT test fixtures
- Contains tiles with known errors: wrong winding, unclosed rings, invalid
  commands, degenerate geometry
- Use for encoder unit testing: your encoder should match valid fixtures and
  reject invalid patterns

**`tippecanoe-decode`**
- Decodes MVT to GeoJSON, warns about winding order and closure problems
- Quick spot-check tool, not a batch validator

### Implementation plan

1. **Batch validation script** (Node.js):
   - Read all tiles from elivagar's PMTiles output
   - Decompress each tile (gzip)
   - Pass raw protobuf buffer to `vtvalidate`
   - Collect all errors with tile coordinates
   - Report: tile count, error count, error categories

2. **Targeted geometry checks** beyond what `vtvalidate` covers:
   - Calculate signed area of every polygon ring to verify winding order
     (CW exterior, CCW interior in screen coords)
   - Check that no polygon has zero-area rings after dedup
   - Check coordinate ranges (all within [-extent, 2*extent] typically;
     values outside [0, extent] are valid for buffer but unusual)
   - Check for overlapping features within the same layer at the same tile

3. **Encoder unit test suite** using `mvt-fixtures`:
   - For each valid fixture: decode the reference tile, re-encode with
     elivagar's encoder, validate the output matches
   - For each known-invalid pattern: verify elivagar's encoder does not
     produce it

### What to look for

Common MVT spec violations that SVG/JSTS tolerate but MapLibre does not:

| Issue | SVG impact | MapLibre impact |
|---|---|---|
| Wrong winding order | None (evenodd fill) | Exterior/interior rings swapped → holes appear filled, fills appear hollow |
| Degenerate polygon rings (< 3 points) | Ignored | May crash or produce artifacts |
| Invalid command sequence (e.g., LineTo before MoveTo) | Decoder may skip | Undefined behavior in GPU shader |
| Coordinates outside extent | Clipped by viewBox | May produce visible overdraw/flicker |
| Multiple geometry types in one layer | Rendered per-type | May confuse layer-level type assumption |
| Feature ID conflicts | No effect | May cause feature-state bugs |

---

## Recommended Execution Order

1. **Approach 5 (Compliance validation)** — Fastest to set up. Write a 50-line
   Node.js script, run over all tiles, see if any fail. If they do, you have
   concrete spec violations to fix. Can be done in an afternoon.

2. **Approach 1 (Encoder swap)** — Clean bisection of the problem. 1-2 days.
   Start with Option A (protobuf-level swap).

3. **Approach 2 (Binary diff)** — If approach 1 clears the encoder, this tells
   you what's different in the data. Pairs well with approach 5 findings.

4. **Approach 4 (Visual regression)** — Set up in parallel with the above.
   Even before you fix the bug, this gives you automated detection of which
   tiles are affected, and later prevents regressions.

5. **Approach 3 (Round-trip re-encode)** — Only needed if approaches 1-2 don't
   isolate the problem. The decode→re-encode cycle helps if the issue is in
   subtle protobuf byte-level encoding that validators don't catch.

---

## Quick-Win Checklist

Before doing any of the above, verify these in 30 minutes:

- [ ] Run `tippecanoe-decode` on a few tiles from the elivagar output — does it
  warn about anything?
- [ ] Check if elivagar's `encode_polygon` respects MVT winding order (CW
  exterior, CCW interior in screen coordinates). The SVG renderer uses
  `evenodd` fill-rule which is winding-agnostic — if the polygon encoder
  doesn't enforce winding, SVG would look fine but MapLibre would not.
- [ ] Compare the protobuf field order in elivagar's encoder vs the MVT spec.
  Some decoders assume fields appear in field-number order (1, 2, 3, 4, 5…).
  Elivagar writes: version (15), name (1), extent (5), features (2), keys (3),
  values (4). That's 15, 1, 5, 2, 3, 4 — not monotonically increasing. A
  strict decoder could choke on this.
- [ ] Verify that the `extent` field value (4096) in the protobuf matches the
  coordinate space used by `encode_polygon`/`encode_linestring`. If geometry
  coordinates are in [0, 8192] but extent says 4096, MapLibre would render
  everything at 2x scale with overflow.
